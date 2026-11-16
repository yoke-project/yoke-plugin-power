//! The power Plugin: the supply, the batteries and the power profile, as questions, a command, streams
//! and occurrences.
//!
//! - **`power.status` is a question and a stream of the same record**, with a sequence: whether the host
//!   runs on battery, the battery's charge, state, time left and warning level, and the profile. A client
//!   that subscribes and then asks holds the present state, and drops what the stream brings at or below
//!   the sequence it was answered with. A change is counted once, whether or not the stream flows.
//! - **`power.devices`** is every device UPower knows — the supply, the batteries, a mouse's or a
//!   headset's — whole, at every change.
//! - **The warning is UPower's**, so the host's own thresholds decide what is low and what is critical:
//!   `power.battery.low` and `power.battery.critical` are reported when the level rises to them,
//!   `power.supply.changed` when the host goes onto or off its battery.
//! - **Every occurrence carries its facts as JSON in its detail**, which is what an interface client
//!   receives; the line is for the log.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use yoke_sdk::plugin::{Capability, Command, Declaration, Event, Object, Outcome, Question, Severity, Stream, Unit};

use crate::backend::{Device, PowerBackend, PowerError, Profiles, Supply};

pub const PLUGIN: &str = "com.yoke.power";
/// The format every answer, sample and detail states as `schema`.
pub const SCHEMA: u32 = 1;

pub const STATUS: &str = "power.status";
pub const DEVICES: &str = "power.devices";
pub const PROFILES: &str = "power.profiles";
pub const QUERIES: [&str; 3] = [STATUS, DEVICES, PROFILES];

pub const PROFILE_SET: &str = "power.profile.set";
pub const COMMANDS: [&str; 1] = [PROFILE_SET];

pub const STREAMS: [&str; 2] = [STATUS, DEVICES];

pub const SUPPLY_CHANGED: &str = "power.supply.changed";
pub const BATTERY_LOW: &str = "power.battery.low";
pub const BATTERY_CRITICAL: &str = "power.battery.critical";
pub const OCCURRENCES: [&str; 3] = [SUPPLY_CHANGED, BATTERY_LOW, BATTERY_CRITICAL];

/// The longest a command waits before it is failed: a profile is set at once or not at all.
pub const COMMAND_WAIT: Duration = Duration::from_secs(20);

pub fn declaration() -> Declaration {
    let answer = |q: &&str| Capability { name: format!("query.{q}.answer"), governs: Object::Query(q.to_string()) };
    let accept = |c: &&str| Capability { name: format!("command.{c}.accept"), governs: Object::Command(c.to_string()) };
    let publish = |s: &&str| Capability { name: format!("stream.{s}.publish"), governs: Object::Stream(s.to_string()) };
    let report = |o: &&str| Capability { name: format!("event.{o}.report"), governs: Object::Occurrence(o.to_string()) };
    Declaration {
        id: PLUGIN.into(),
        queries: QUERIES.iter().map(|q| q.to_string()).collect(),
        commands: COMMANDS.iter().map(|c| c.to_string()).collect(),
        streams: vec![
            // A change of the supply is not one a client may miss.
            Stream { id: STATUS.into(), tolerates_loss: false, tolerates_reorder: false, max_frame: 0 },
            // The whole picture again each time, so a lost one is made good by the next.
            Stream { id: DEVICES.into(), tolerates_loss: true, tolerates_reorder: false, max_frame: 0 },
        ],
        occurrences: OCCURRENCES.iter().map(|o| o.to_string()).collect(),
        capabilities: QUERIES
            .iter()
            .map(answer)
            .chain(COMMANDS.iter().map(accept))
            .chain(STREAMS.iter().map(publish))
            .chain(OCCURRENCES.iter().map(report))
            .collect(),
        ..Default::default()
    }
}

#[derive(Debug)]
pub enum Error {
    Invalid(String),
    Power(PowerError),
}

impl From<PowerError> for Error {
    fn from(e: PowerError) -> Self {
        Error::Power(e)
    }
}

pub fn code(e: &Error) -> &'static str {
    match e {
        Error::Invalid(_) => "invalid",
        Error::Power(PowerError::Unavailable(_)) => "unavailable",
        Error::Power(PowerError::NotFound(_)) => "not_found",
        Error::Power(PowerError::PermissionDenied) => "permission_denied",
        Error::Power(PowerError::Bus(_)) => "backend",
    }
}

pub fn message(e: &Error) -> String {
    match e {
        Error::Invalid(m) => m.clone(),
        Error::Power(e) => e.to_string(),
    }
}

fn input<T: DeserializeOwned>(what: &str, payload: &[u8]) -> Result<T, Error> {
    let payload = if payload.iter().all(u8::is_ascii_whitespace) { b"{}".as_slice() } else { payload };
    serde_json::from_slice(payload).map_err(|e| {
        let kind = match e.classify() {
            serde_json::error::Category::Data => "a field is missing, unknown or of the wrong type",
            _ => "it is not JSON",
        };
        Error::Invalid(format!("the payload is not what {what} takes: {kind}, at line {} column {}", e.line(), e.column()))
    })
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NoInput {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileInput {
    pub profile: String,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

pub fn device_json(d: &Device) -> Value {
    json!({
        "id": d.id, "kind": d.kind, "vendor": d.vendor, "model": d.model, "power_supply": d.power_supply,
        "present": d.present, "online": d.online, "percentage": d.percentage, "state": d.state,
        "time_to_empty": d.time_to_empty, "time_to_full": d.time_to_full, "energy_rate": d.energy_rate,
        "warning_level": d.warning_level,
    })
}

fn profiles_json(p: &Profiles) -> Value {
    json!({ "active": p.active, "available": p.available, "degraded": p.degraded })
}

/// The status record's body: what is compared to decide whether something changed. The percentage is
/// kept to a tenth, so that UPower's last decimal does not count as a change.
fn body(s: &Supply, profile: Option<&Profiles>) -> Value {
    let b = &s.battery;
    json!({
        "on_battery": s.on_battery,
        "battery": b.present.then(|| json!({
            "percentage": (b.percentage * 10.0).round() / 10.0,
            "state": b.state,
            "time_to_empty": b.time_to_empty,
            "time_to_full": b.time_to_full,
            "warning_level": b.warning_level,
        })),
        "profile": profile.map(|p| p.active.clone()),
    })
}

/// The warning level as a rank: what is reported when it rises.
fn rank(level: &str) -> u8 {
    match level {
        "low" => 1,
        "critical" | "action" => 2,
        _ => 0,
    }
}

pub trait Acts {
    fn answer(&self, q: &Question, payload: Vec<u8>) -> impl std::future::Future<Output = Result<(), yoke_sdk::base::Error>>;
    fn fail(&self, q: &Question, code: &str, message: &str) -> impl std::future::Future<Output = Result<(), yoke_sdk::base::Error>>;
    fn ack(&self, c: &Command, outcome: Outcome, line: &str) -> impl std::future::Future<Output = Result<(), yoke_sdk::base::Error>>;
    /// An occurrence, its facts as JSON in its detail.
    fn report(&self, occurrence: &str, severity: u8, line: &str, detail: Value) -> impl std::future::Future<Output = Result<(), yoke_sdk::base::Error>>;
    fn emit(&self, stream: &str, payload: Vec<u8>) -> impl std::future::Future<Output = Result<(), yoke_sdk::base::Error>>;
}

impl Acts for Unit {
    async fn answer(&self, q: &Question, payload: Vec<u8>) -> Result<(), yoke_sdk::base::Error> {
        Unit::answer(self, q, payload).await
    }
    async fn fail(&self, q: &Question, code: &str, message: &str) -> Result<(), yoke_sdk::base::Error> {
        Unit::fail(self, &q.id, code, message).await
    }
    async fn ack(&self, c: &Command, outcome: Outcome, line: &str) -> Result<(), yoke_sdk::base::Error> {
        Unit::ack(self, c, outcome, line).await
    }
    async fn report(&self, occurrence: &str, severity: u8, line: &str, detail: Value) -> Result<(), yoke_sdk::base::Error> {
        Unit::report(self, occurrence, Some(Severity::of(severity)), line, detail.to_string().into_bytes()).await
    }
    async fn emit(&self, stream: &str, payload: Vec<u8>) -> Result<(), yoke_sdk::base::Error> {
        Unit::emit(self, stream, payload).await
    }
}

struct Status {
    sequence: u64,
    body: Value,
    at: u64,
}

impl Status {
    fn record(&self) -> Value {
        let mut v = json!({ "schema": SCHEMA, "sequence": self.sequence, "at": self.at });
        if let (Some(v), Some(body)) = (v.as_object_mut(), self.body.as_object()) {
            v.extend(body.clone());
        }
        v
    }
}

pub type Backend = Result<Arc<dyn PowerBackend>, PowerError>;

pub struct Power {
    pub backend: Backend,
    status: Mutex<Status>,
    devices: Mutex<String>,
    status_flows: AtomicBool,
    devices_flow: AtomicBool,
}

impl Power {
    pub fn new(backend: Backend) -> Arc<Power> {
        Arc::new(Power {
            backend,
            status: Mutex::new(Status { sequence: 0, body: Value::Null, at: now_ms() }),
            devices: Mutex::new(String::new()),
            status_flows: AtomicBool::new(false),
            devices_flow: AtomicBool::new(false),
        })
    }

    fn upower(&self) -> Result<&dyn PowerBackend, Error> {
        match &self.backend {
            Ok(b) => Ok(b.as_ref()),
            Err(e) => Err(Error::Power(PowerError::Unavailable(e.to_string()))),
        }
    }

    fn flag(&self, stream: &str) -> Option<&AtomicBool> {
        match stream {
            STATUS => Some(&self.status_flows),
            DEVICES => Some(&self.devices_flow),
            _ => None,
        }
    }

    /// Reads everything again and says what changed: the status counted, emitted where it flows, and the
    /// occurrences its change makes; the devices emitted where they flow and changed. The first reading
    /// counts as sequence 1 and reports nothing.
    pub async fn refresh(&self, u: &impl Acts) -> Result<(), yoke_sdk::base::Error> {
        let Ok(b) = &self.backend else { return Ok(()) };
        let Ok(supply) = b.supply().await else { return Ok(()) };
        let profiles = b.profiles().await.ok();
        let next = body(&supply, profiles.as_ref());
        let mut first = Ok(());
        let changed = {
            let mut status = self.status.lock().unwrap();
            if status.body == next {
                None
            } else {
                let before = std::mem::replace(&mut status.body, next.clone());
                status.sequence += 1;
                status.at = now_ms();
                Some((before, status.record()))
            }
        };
        if let Some((before, record)) = changed {
            if self.status_flows.load(Ordering::Relaxed) {
                first = u.emit(STATUS, record.to_string().into_bytes()).await;
            }
            if !before.is_null() {
                first = first.and(self.occurrences(u, &before, &next, &supply).await);
            }
        }
        if self.devices_flow.load(Ordering::Relaxed) {
            if let Ok(devices) = b.devices().await {
                first = first.and(self.emit_devices(u, &devices, false).await);
            }
        }
        first
    }

    async fn occurrences(&self, u: &impl Acts, before: &Value, now: &Value, supply: &Supply) -> Result<(), yoke_sdk::base::Error> {
        let mut first = Ok(());
        let percent = supply.battery.present.then_some(supply.battery.percentage.round() as i64);
        if before["on_battery"] != now["on_battery"] {
            let (line, severity) =
                if supply.on_battery { ("on battery", 30) } else { ("on external power", 10) };
            let line = match percent {
                Some(p) => format!("{line}, the battery at {p}%"),
                None => line.to_string(),
            };
            let detail = json!({ "schema": SCHEMA, "line": line, "on_battery": supply.on_battery, "percentage": percent });
            first = u.report(SUPPLY_CHANGED, severity, &line, detail).await;
        }
        let level = |v: &Value| v["battery"]["warning_level"].as_str().map(str::to_string).unwrap_or_default();
        let (was, is) = (rank(&level(before)), rank(&level(now)));
        if is > was {
            let (occurrence, severity, word) =
                if is == 2 { (BATTERY_CRITICAL, 70, "critical") } else { (BATTERY_LOW, 50, "low") };
            let left = supply.battery.time_to_empty;
            let line = match (percent, left) {
                (Some(p), t) if t > 0 => format!("the battery is {word}: {p}%, about {} min left", t / 60),
                (Some(p), _) => format!("the battery is {word}: {p}%"),
                _ => format!("the battery is {word}"),
            };
            let detail = json!({
                "schema": SCHEMA, "line": line, "level": supply.battery.warning_level,
                "percentage": percent, "time_to_empty": left,
            });
            first = first.and(u.report(occurrence, severity, &line, detail).await);
        }
        first
    }

    async fn emit_devices(&self, u: &impl Acts, devices: &[Device], always: bool) -> Result<(), yoke_sdk::base::Error> {
        let list: Vec<Value> = devices.iter().map(device_json).collect();
        let picture = Value::Array(list.clone()).to_string();
        {
            let mut last = self.devices.lock().unwrap();
            if !always && *last == picture {
                return Ok(());
            }
            *last = picture;
        }
        u.emit(DEVICES, json!({ "schema": SCHEMA, "at": now_ms(), "devices": list }).to_string().into_bytes()).await
    }

    /// A stream was activated: it is given the present picture at once.
    pub async fn activated(&self, u: &impl Acts, stream: &str) -> Result<(), yoke_sdk::base::Error> {
        let Some(flag) = self.flag(stream) else { return Ok(()) };
        flag.store(true, Ordering::Relaxed);
        match stream {
            STATUS => {
                let record = self.status.lock().unwrap().record();
                u.emit(STATUS, record.to_string().into_bytes()).await
            }
            _ => match &self.backend {
                Ok(b) => match b.devices().await {
                    Ok(d) => self.emit_devices(u, &d, true).await,
                    Err(_) => Ok(()),
                },
                Err(_) => Ok(()),
            },
        }
    }

    pub fn stopped(&self, stream: &str) {
        if let Some(flag) = self.flag(stream) {
            flag.store(false, Ordering::Relaxed);
        }
    }

    pub async fn answer(&self, query: &str, payload: &[u8]) -> Result<Vec<u8>, Error> {
        let _: NoInput = input(query, payload)?;
        let v = match query {
            STATUS => {
                self.upower()?;
                self.status.lock().unwrap().record()
            }
            DEVICES => {
                let devices = self.upower()?.devices().await?;
                json!({ "schema": SCHEMA, "devices": devices.iter().map(device_json).collect::<Vec<_>>() })
            }
            PROFILES => {
                let p = self.upower()?.profiles().await?;
                let mut v = profiles_json(&p);
                v["schema"] = json!(SCHEMA);
                v
            }
            other => return Err(Error::Invalid(format!("{other} is not a question of {PLUGIN}"))),
        };
        Ok(v.to_string().into_bytes())
    }

    pub async fn act(&self, command: &str, payload: &[u8]) -> Result<String, Error> {
        match command {
            PROFILE_SET => {
                let i: ProfileInput = input(command, payload)?;
                self.upower()?.set_profile(&i.profile).await?;
                Ok(format!("the profile is {}", i.profile))
            }
            other => Err(Error::Invalid(format!("{other} is not a command of {PLUGIN}"))),
        }
    }
}

pub async fn handle(u: &impl Acts, event: &Event, p: &Power) -> Result<(), yoke_sdk::base::Error> {
    match event {
        Event::Question(q) if QUERIES.contains(&q.r#type.as_str()) => match p.answer(&q.r#type, &q.payload).await {
            Ok(answer) => u.answer(q, answer).await,
            Err(e) => u.fail(q, code(&e), &message(&e)).await,
        },
        Event::Command(c) if COMMANDS.contains(&c.r#type.as_str()) => {
            match tokio::time::timeout(COMMAND_WAIT, p.act(&c.r#type, &c.payload)).await {
                Ok(Ok(line)) => u.ack(c, Outcome::Done, &line).await,
                Ok(Err(e)) => u.ack(c, Outcome::Failed, &format!("{}: {}", code(&e), message(&e))).await,
                Err(_) => u.ack(c, Outcome::Failed, "backend: the profile daemon did not answer").await,
            }
        }
        _ => Ok(()),
    }
}
