//! UPower and power-profiles-daemon over the system bus.
//!
//! [`Upower`] reads the supply and every device UPower knows, and says when any of them changed. The
//! profile is power-profiles-daemon's, under either of the two names it has carried; a host without it
//! has no profile, and says so.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt;
use tokio::sync::broadcast;
use zbus::Connection;
use zvariant::{OwnedObjectPath, OwnedValue};

const UPOWER: &str = "org.freedesktop.UPower";
const UPOWER_PATH: &str = "/org/freedesktop/UPower";
const DEVICE: &str = "org.freedesktop.UPower.Device";
/// power-profiles-daemon's bus names, the current one first.
const PROFILES: [(&str, &str); 2] = [
    ("org.freedesktop.UPower.PowerProfiles", "/org/freedesktop/UPower/PowerProfiles"),
    ("net.hadess.PowerProfiles", "/net/hadess/PowerProfiles"),
];

#[derive(Debug)]
pub enum PowerError {
    /// The service is not on the bus.
    Unavailable(String),
    /// The host has no such thing: no profile daemon, no such profile.
    NotFound(String),
    /// polkit said no.
    PermissionDenied,
    Bus(String),
}

impl std::fmt::Display for PowerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PowerError::Unavailable(m) => write!(f, "unavailable: {m}"),
            PowerError::NotFound(m) => write!(f, "{m}"),
            PowerError::PermissionDenied => write!(f, "the account may not do it (polkit)"),
            PowerError::Bus(m) => write!(f, "{m}"),
        }
    }
}

impl From<zbus::Error> for PowerError {
    fn from(e: zbus::Error) -> Self {
        let s = e.to_string();
        if s.contains("AccessDenied") || s.contains("NotAuthorized") || s.contains("PermissionDenied") {
            PowerError::PermissionDenied
        } else {
            PowerError::Bus(s)
        }
    }
}

impl From<zbus::fdo::Error> for PowerError {
    fn from(e: zbus::fdo::Error) -> Self {
        PowerError::from(zbus::Error::from(e))
    }
}

/// One device UPower knows: the supply, a battery, a mouse's battery, a UPS.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Device {
    /// UPower's name for it, stable while it is plugged: `battery_BAT0`, `line_power_AC`.
    pub id: String,
    pub kind: &'static str,
    pub vendor: String,
    pub model: String,
    /// It powers the host, rather than being a peripheral's battery.
    pub power_supply: bool,
    pub present: bool,
    /// A supply: plugged in.
    pub online: bool,
    pub percentage: f64,
    pub state: &'static str,
    /// Seconds; zero where UPower does not know.
    pub time_to_empty: i64,
    pub time_to_full: i64,
    /// Watts.
    pub energy_rate: f64,
    pub warning_level: &'static str,
}

/// The supply as a whole: UPower's display device, and whether the host runs on battery.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Supply {
    pub on_battery: bool,
    /// The composite battery; `present` is false on a host with none.
    pub battery: Device,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Profiles {
    pub active: String,
    pub available: Vec<String>,
    /// Why the performance profile is held back, where it is: `lap-detected`, `high-operating-temperature`.
    pub degraded: String,
}

#[async_trait]
pub trait PowerBackend: Send + Sync + 'static {
    async fn supply(&self) -> Result<Supply, PowerError>;
    async fn devices(&self) -> Result<Vec<Device>, PowerError>;
    async fn profiles(&self) -> Result<Profiles, PowerError>;
    async fn set_profile(&self, profile: &str) -> Result<(), PowerError>;
    /// Says that something changed, without saying what: the caller reads again.
    fn subscribe(&self) -> broadcast::Receiver<()>;
}

pub fn kind(n: u32) -> &'static str {
    const KINDS: [&str; 30] = [
        "unknown", "line-power", "battery", "ups", "monitor", "mouse", "keyboard", "pda", "phone", "media-player",
        "tablet", "computer", "gaming-input", "pen", "touchpad", "modem", "network", "headset", "speakers",
        "headphones", "video", "other-audio", "remote-control", "printer", "scanner", "camera", "wearable", "toy",
        "bluetooth-generic", "unknown",
    ];
    KINDS.get(n as usize).copied().unwrap_or("unknown")
}

pub fn state(n: u32) -> &'static str {
    match n {
        1 => "charging",
        2 => "discharging",
        3 => "empty",
        4 => "fully-charged",
        5 => "pending-charge",
        6 => "pending-discharge",
        _ => "unknown",
    }
}

pub fn warning(n: u32) -> &'static str {
    match n {
        1 => "none",
        2 => "discharging",
        3 => "low",
        4 => "critical",
        5 => "action",
        _ => "unknown",
    }
}

fn get<T: TryFrom<OwnedValue>>(m: &HashMap<String, OwnedValue>, key: &str) -> Option<T> {
    m.get(key).and_then(|v| v.try_clone().ok()).and_then(|v| T::try_from(v).ok())
}

pub struct Upower {
    conn: Connection,
    changed: broadcast::Sender<()>,
}

impl Upower {
    /// Finds UPower on the system bus, and starts listening for its changes.
    pub async fn connect() -> Result<Arc<Upower>, PowerError> {
        let conn = Connection::system().await.map_err(|e| PowerError::Unavailable(format!("the system bus: {e}")))?;
        let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
        let names: Vec<String> = dbus.list_names().await?.into_iter().map(|n| n.to_string()).collect();
        if !names.iter().any(|n| n == UPOWER) {
            return Err(PowerError::Unavailable("UPower is not on the system bus".into()));
        }
        let (changed, _) = broadcast::channel(16);
        let me = Arc::new(Upower { conn, changed });
        let (conn, tx) = (me.conn.clone(), me.changed.clone());
        tokio::spawn(async move {
            let _ = listen(conn, tx).await;
        });
        Ok(me)
    }

    async fn device(&self, path: &OwnedObjectPath) -> Result<Device, PowerError> {
        let props = zbus::fdo::PropertiesProxy::builder(&self.conn).destination(UPOWER)?.path(path.as_str())?.build().await?;
        let m = props.get_all(zbus::names::InterfaceName::try_from(DEVICE).map_err(|e| PowerError::Bus(e.to_string()))?).await?;
        let id = path.as_str().rsplit('/').next().unwrap_or_default().to_string();
        Ok(Device {
            id,
            kind: kind(get(&m, "Type").unwrap_or(0)),
            vendor: get(&m, "Vendor").unwrap_or_default(),
            model: get(&m, "Model").unwrap_or_default(),
            power_supply: get(&m, "PowerSupply").unwrap_or(false),
            present: get(&m, "IsPresent").unwrap_or(false),
            online: get(&m, "Online").unwrap_or(false),
            percentage: get(&m, "Percentage").unwrap_or(0.0),
            state: state(get(&m, "State").unwrap_or(0)),
            time_to_empty: get(&m, "TimeToEmpty").unwrap_or(0),
            time_to_full: get(&m, "TimeToFull").unwrap_or(0),
            energy_rate: get(&m, "EnergyRate").unwrap_or(0.0),
            warning_level: warning(get(&m, "WarningLevel").unwrap_or(0)),
        })
    }

    /// power-profiles-daemon, under whichever name this host carries it.
    async fn profiles_proxy(&self) -> Result<(zbus::fdo::PropertiesProxy<'static>, &'static str), PowerError> {
        let dbus = zbus::fdo::DBusProxy::new(&self.conn).await?;
        let names: Vec<String> = dbus.list_names().await?.into_iter().map(|n| n.to_string()).collect();
        let activatable: Vec<String> =
            dbus.list_activatable_names().await?.into_iter().map(|n| n.to_string()).collect();
        for (bus, path) in PROFILES {
            if names.iter().chain(activatable.iter()).any(|n| n == bus) {
                let p = zbus::fdo::PropertiesProxy::builder(&self.conn).destination(bus)?.path(path)?.build().await?;
                return Ok((p, bus));
            }
        }
        Err(PowerError::NotFound("this host has no power profile daemon".into()))
    }
}

/// Every PropertiesChanged UPower and the profile daemon send, and every device added or removed, is one
/// "something changed".
async fn listen(conn: Connection, tx: broadcast::Sender<()>) -> zbus::Result<()> {
    let mut streams = Vec::new();
    for (sender, namespace) in [(UPOWER, UPOWER_PATH), (PROFILES[0].0, PROFILES[0].1), (PROFILES[1].0, PROFILES[1].1)] {
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(sender)?
            .path_namespace(namespace)?
            .build();
        streams.push(zbus::MessageStream::for_match_rule(rule, &conn, None).await?);
    }
    let mut all = futures_util::stream::select_all(streams);
    while let Some(msg) = all.next().await {
        if msg.is_ok() {
            let _ = tx.send(());
        }
    }
    Ok(())
}

#[async_trait]
impl PowerBackend for Upower {
    async fn supply(&self) -> Result<Supply, PowerError> {
        let proxy = zbus::Proxy::new(&self.conn, UPOWER, UPOWER_PATH, UPOWER).await?;
        let on_battery: bool = proxy.get_property("OnBattery").await?;
        let display: OwnedObjectPath = proxy.call("GetDisplayDevice", &()).await?;
        let mut battery = self.device(&display).await?;
        battery.id = "display".into();
        Ok(Supply { on_battery, battery })
    }

    async fn devices(&self) -> Result<Vec<Device>, PowerError> {
        let proxy = zbus::Proxy::new(&self.conn, UPOWER, UPOWER_PATH, UPOWER).await?;
        let paths: Vec<OwnedObjectPath> = proxy.call("EnumerateDevices", &()).await?;
        let mut out = Vec::with_capacity(paths.len());
        for p in &paths {
            // A device removed between the two calls is not an error.
            if let Ok(d) = self.device(p).await {
                out.push(d);
            }
        }
        Ok(out)
    }

    async fn profiles(&self) -> Result<Profiles, PowerError> {
        let (p, bus) = self.profiles_proxy().await?;
        let iface = zbus::names::InterfaceName::try_from(bus).map_err(|e| PowerError::Bus(e.to_string()))?;
        let m = p.get_all(iface).await?;
        let available: Vec<HashMap<String, OwnedValue>> = get(&m, "Profiles").unwrap_or_default();
        Ok(Profiles {
            active: get(&m, "ActiveProfile").unwrap_or_default(),
            available: available.iter().filter_map(|e| get::<String>(e, "Profile")).collect(),
            degraded: get(&m, "PerformanceDegraded").unwrap_or_default(),
        })
    }

    async fn set_profile(&self, profile: &str) -> Result<(), PowerError> {
        let known = self.profiles().await?;
        if !known.available.iter().any(|p| p == profile) {
            return Err(PowerError::NotFound(format!(
                "no profile {profile}: this host offers {}",
                known.available.join(", ")
            )));
        }
        let (p, bus) = self.profiles_proxy().await?;
        let iface = zbus::names::InterfaceName::try_from(bus).map_err(|e| PowerError::Bus(e.to_string()))?;
        p.set(iface, "ActiveProfile", zvariant::Value::from(profile)).await?;
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<()> {
        self.changed.subscribe()
    }
}
