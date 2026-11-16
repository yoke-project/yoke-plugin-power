//! The Plugin against a UPower that is not there: a fake backend stands in for it, and a recorder for
//! the Core.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use power::backend::{Device, PowerBackend, PowerError, Profiles, Supply};
use power::plugin::*;
use serde_json::Value;
use tokio::sync::broadcast;
use yoke_sdk::plugin::{Command, Event, Outcome, Question};

fn battery(percentage: f64, state: &'static str, warning_level: &'static str) -> Device {
    Device {
        id: "display".into(),
        kind: "battery",
        power_supply: true,
        present: true,
        percentage,
        state,
        time_to_empty: 3600,
        warning_level,
        ..Default::default()
    }
}

/// A laptop: a battery, a supply, a mouse's battery, and a profile daemon with three profiles.
struct Fake {
    supply: Mutex<Supply>,
    profile: Mutex<String>,
    has_profiles: bool,
    changed: broadcast::Sender<()>,
}

impl Fake {
    fn new() -> Arc<Fake> {
        Arc::new(Fake {
            supply: Mutex::new(Supply { on_battery: false, battery: battery(80.04, "charging", "none") }),
            profile: Mutex::new("balanced".into()),
            has_profiles: true,
            changed: broadcast::channel(4).0,
        })
    }
    fn set(&self, on_battery: bool, percentage: f64, warning: &'static str) {
        let state = if on_battery { "discharging" } else { "charging" };
        *self.supply.lock().unwrap() = Supply { on_battery, battery: battery(percentage, state, warning) };
    }
}

#[async_trait]
impl PowerBackend for Fake {
    async fn supply(&self) -> Result<Supply, PowerError> {
        Ok(self.supply.lock().unwrap().clone())
    }
    async fn devices(&self) -> Result<Vec<Device>, PowerError> {
        let mut b = self.supply.lock().unwrap().battery.clone();
        b.id = "battery_BAT0".into();
        let ac = Device { id: "line_power_AC".into(), kind: "line-power", power_supply: true, online: true, ..Default::default() };
        let mouse = Device { id: "mouse_1".into(), kind: "mouse", present: true, percentage: 40.0, ..Default::default() };
        Ok(vec![b, ac, mouse])
    }
    async fn profiles(&self) -> Result<Profiles, PowerError> {
        if !self.has_profiles {
            return Err(PowerError::NotFound("this host has no power profile daemon".into()));
        }
        Ok(Profiles {
            active: self.profile.lock().unwrap().clone(),
            available: vec!["power-saver".into(), "balanced".into(), "performance".into()],
            degraded: String::new(),
        })
    }
    async fn set_profile(&self, profile: &str) -> Result<(), PowerError> {
        if !["power-saver", "balanced", "performance"].contains(&profile) {
            return Err(PowerError::NotFound(format!("no profile {profile}")));
        }
        *self.profile.lock().unwrap() = profile.into();
        Ok(())
    }
    fn subscribe(&self) -> broadcast::Receiver<()> {
        self.changed.subscribe()
    }
}

#[derive(Default)]
struct Core {
    said: Mutex<Vec<String>>,
}

impl Core {
    fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.said.lock().unwrap())
    }
    fn push(&self, line: String) -> Result<(), yoke_sdk::base::Error> {
        self.said.lock().unwrap().push(line);
        Ok(())
    }
}

impl Acts for Core {
    async fn answer(&self, _q: &Question, payload: Vec<u8>) -> Result<(), yoke_sdk::base::Error> {
        self.push(format!("answer {}", String::from_utf8_lossy(&payload)))
    }
    async fn fail(&self, _q: &Question, code: &str, message: &str) -> Result<(), yoke_sdk::base::Error> {
        self.push(format!("fail {code}: {message}"))
    }
    async fn ack(&self, _c: &Command, outcome: Outcome, line: &str) -> Result<(), yoke_sdk::base::Error> {
        self.push(format!("ack {outcome:?}: {line}"))
    }
    async fn report(&self, occurrence: &str, severity: u8, line: &str, detail: Value) -> Result<(), yoke_sdk::base::Error> {
        assert_eq!(detail["line"], line, "the detail carries the line a client reads");
        self.push(format!("report {occurrence} {severity}: {line}"))
    }
    async fn emit(&self, stream: &str, payload: Vec<u8>) -> Result<(), yoke_sdk::base::Error> {
        self.push(format!("emit {stream} {}", String::from_utf8_lossy(&payload)))
    }
}

fn question(r#type: &str, payload: &str) -> Event {
    Event::Question(Question { id: "q-1".into(), r#type: r#type.into(), payload: payload.as_bytes().to_vec() })
}

fn command(r#type: &str, payload: &str) -> Event {
    Event::Command(Command { id: "c-1".into(), r#type: r#type.into(), payload: payload.as_bytes().to_vec() })
}

fn json_of(line: &str, skip: usize) -> Value {
    serde_json::from_str(line.splitn(skip + 1, ' ').nth(skip).unwrap()).unwrap()
}

#[test]
fn the_manifest_kept_is_the_manifest_declared() {
    assert_eq!(declaration().manifest(), include_str!("../com.yoke.power.yaml"));
}

#[test]
fn every_object_declared_has_one_capability() {
    let d = declaration();
    assert_eq!(d.capabilities.len(), d.queries.len() + d.commands.len() + d.streams.len() + d.occurrences.len());
}

#[tokio::test]
async fn the_status_is_counted_once_per_change_and_a_tenth_of_a_percent_is_none() {
    let (fake, core) = (Fake::new(), Core::default());
    let p = Power::new(Ok(fake.clone()));
    p.refresh(&core).await.unwrap();
    assert!(core.take().is_empty(), "the first reading reports nothing");
    handle(&core, &question(STATUS, ""), &p).await.unwrap();
    let s = json_of(&core.take()[0], 1);
    assert_eq!(s["sequence"], 1);
    assert_eq!(s["on_battery"], false);
    assert_eq!(s["battery"]["percentage"], 80.0);
    assert_eq!(s["profile"], "balanced");

    fake.set(false, 80.01, "none");
    *fake.supply.lock().unwrap() = Supply { on_battery: false, battery: battery(80.01, "charging", "none") };
    p.refresh(&core).await.unwrap();
    handle(&core, &question(STATUS, ""), &p).await.unwrap();
    assert_eq!(json_of(&core.take()[0], 1)["sequence"], 1, "UPower's last decimal is no change");
}

#[tokio::test]
async fn going_onto_the_battery_and_running_low_are_reported_once_each() {
    let (fake, core) = (Fake::new(), Core::default());
    let p = Power::new(Ok(fake.clone()));
    p.refresh(&core).await.unwrap();
    p.activated(&core, STATUS).await.unwrap();
    core.take();

    fake.set(true, 79.6, "discharging");
    p.refresh(&core).await.unwrap();
    let said = core.take();
    assert_eq!(json_of(&said[0], 2)["sequence"], 2);
    assert_eq!(said[1], format!("report {SUPPLY_CHANGED} 30: on battery, the battery at 80%"));

    fake.set(true, 9.0, "low");
    p.refresh(&core).await.unwrap();
    assert_eq!(core.take()[1], format!("report {BATTERY_LOW} 50: the battery is low: 9%, about 60 min left"));
    // Still low: counted as a change of charge, not reported again.
    fake.set(true, 8.0, "low");
    p.refresh(&core).await.unwrap();
    assert_eq!(core.take().len(), 1);

    fake.set(true, 3.0, "critical");
    p.refresh(&core).await.unwrap();
    assert_eq!(core.take()[1], format!("report {BATTERY_CRITICAL} 70: the battery is critical: 3%, about 60 min left"));

    // Plugged in: the supply is reported, and a level that falls is not.
    fake.set(false, 3.5, "none");
    p.refresh(&core).await.unwrap();
    let said = core.take();
    assert_eq!(said.len(), 2, "{said:?}");
    assert_eq!(said[1], format!("report {SUPPLY_CHANGED} 10: on external power, the battery at 4%"));
}

#[tokio::test]
async fn the_profile_is_set_and_an_unknown_one_refused() {
    let (fake, core) = (Fake::new(), Core::default());
    let p = Power::new(Ok(fake.clone()));
    handle(&core, &command(PROFILE_SET, r#"{"profile":"performance"}"#), &p).await.unwrap();
    handle(&core, &command(PROFILE_SET, r#"{"profile":"turbo"}"#), &p).await.unwrap();
    handle(&core, &command(PROFILE_SET, r#"{"mode":"turbo"}"#), &p).await.unwrap();
    handle(&core, &question(PROFILES, ""), &p).await.unwrap();
    let said = core.take();
    assert_eq!(said[0], "ack Done: the profile is performance");
    assert_eq!(said[1], "ack Failed: not_found: no profile turbo");
    assert!(said[2].starts_with("ack Failed: invalid: the payload is not what power.profile.set takes"), "{said:?}");
    assert_eq!(json_of(&said[3], 1)["active"], "performance");
}

#[tokio::test]
async fn the_devices_flow_whole_and_only_when_changed() {
    let (fake, core) = (Fake::new(), Core::default());
    let p = Power::new(Ok(fake.clone()));
    p.refresh(&core).await.unwrap();
    p.activated(&core, DEVICES).await.unwrap();
    let devices = json_of(&core.take()[0], 2)["devices"].clone();
    assert_eq!(devices.as_array().unwrap().len(), 3);
    assert_eq!(devices[2]["kind"], "mouse");
    p.refresh(&core).await.unwrap();
    assert!(core.take().is_empty());
}

#[tokio::test]
async fn without_upower_every_question_and_command_is_failed_with_the_reason() {
    let core = Core::default();
    let p = Power::new(Err(PowerError::Unavailable("UPower is not on the system bus".into())));
    p.refresh(&core).await.unwrap();
    handle(&core, &question(STATUS, ""), &p).await.unwrap();
    handle(&core, &command(PROFILE_SET, r#"{"profile":"balanced"}"#), &p).await.unwrap();
    let said = core.take();
    assert!(said[0].starts_with("fail unavailable: "), "{said:?}");
    assert!(said[1].starts_with("ack Failed: unavailable: "), "{said:?}");
}

#[tokio::test]
async fn a_host_without_a_profile_daemon_has_a_status_with_no_profile() {
    let core = Core::default();
    let fake = Arc::new(Fake { has_profiles: false, ..Arc::try_unwrap(Fake::new()).ok().unwrap() });
    let p = Power::new(Ok(fake));
    p.refresh(&core).await.unwrap();
    handle(&core, &question(STATUS, ""), &p).await.unwrap();
    handle(&core, &question(PROFILES, ""), &p).await.unwrap();
    let said = core.take();
    assert_eq!(json_of(&said[0], 1)["profile"], Value::Null);
    assert_eq!(said[1], "fail not_found: this host has no power profile daemon");
}
