//! Bridges the window and the power Plugin, through the interface contract alone.
//!
//! - **Reading.** `power.status` is a stream of changes with a sequence: the panel subscribes first, then
//!   asks it, and drops every frame at or below the sequence it was answered with. `power.devices` is the
//!   whole picture at every change, and replaces the list. The profiles are asked once, and again after a
//!   profile is set; the active one comes with the status.
//! - **Acting.** A profile button is `power.profile.set`; its acknowledgement is the message at the top.
//! - **The events** are the Core's events about the unit: its state, its health, the occurrences it
//!   reports — the supply changing, the battery low or critical.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};
use tokio::runtime::Handle;
use tokio::time::{interval, sleep};
use yoke_sdk::interface::subscribed::Carries;
use yoke_sdk::interface::{Attachment, Filter, Options};

use crate::{DeviceRow, MainWindow};

const STATUS: &str = "power.status";
const DEVICES: &str = "power.devices";
const PROFILES: &str = "power.profiles";
const MAX_EVENTS: usize = 300;
const EVENTS_REDRAW: Duration = Duration::from_millis(500);
const AGAIN: Duration = Duration::from_secs(2);

#[derive(Default)]
struct Log {
    lines: VecDeque<String>,
    dirty: bool,
}

pub struct Shared {
    unit: String,
    attachment: OnceLock<Arc<Attachment>>,
    started: Mutex<Vec<String>>,
}

#[derive(Clone)]
struct Ctx {
    at: Arc<Attachment>,
    unit: String,
    ui: Weak<MainWindow>,
    log: Arc<Mutex<Log>>,
    t0: Instant,
    shared: Arc<Shared>,
}

fn say(ui: &Weak<MainWindow>, msg: impl Into<String>) {
    let m = msg.into();
    let _ = ui.upgrade_in_event_loop(move |u| u.set_message(m.into()));
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

/// Seconds as hours and minutes; nothing for zero, which UPower says when it does not know.
fn duration(seconds: i64) -> Option<String> {
    (seconds > 0).then(|| {
        let m = seconds / 60;
        if m >= 60 { format!("{} h {:02} min", m / 60, m % 60) } else { format!("{m} min") }
    })
}

pub fn start(rt: Handle, address: String, unit: String, ui: &MainWindow) -> Arc<Shared> {
    let shared = Arc::new(Shared { unit: unit.clone(), attachment: OnceLock::new(), started: Mutex::new(Vec::new()) });
    let weak = ui.as_weak();
    let log = Arc::new(Mutex::new(Log::default()));
    let ready: Arc<OnceLock<Ctx>> = Arc::new(OnceLock::new());

    {
        let (ready, rt, weak) = (ready.clone(), rt.clone(), weak.clone());
        ui.on_set_profile(move |profile| match ready.get() {
            Some(c) => {
                let c = c.clone();
                rt.spawn(async move {
                    match command(&c, "power.profile.set", json!({ "profile": profile.to_string() })).await {
                        Ok(line) => say(&c.ui, line),
                        Err(e) => say(&c.ui, format!("The profile was not set: {e}")),
                    }
                    profiles(&c).await;
                });
            }
            None => say(&weak, "Not attached to the application yet"),
        });
    }
    {
        let (log, weak) = (log.clone(), weak.clone());
        ui.on_clear_events(move || {
            let mut l = log.lock().unwrap();
            l.lines.clear();
            l.dirty = false;
            let _ = weak.upgrade_in_event_loop(|u| u.set_events(ModelRc::new(VecModel::default())));
        });
    }

    let shared2 = shared.clone();
    rt.spawn(async move {
        let at = match Attachment::attach(&address, Options::new()).await {
            Ok(at) => Arc::new(at),
            Err(e) => {
                say(&weak, format!("Cannot attach to {address}: {e}"));
                return;
            }
        };
        let _ = shared2.attachment.set(at.clone());
        let c = Ctx { at, unit, ui: weak, log, t0: Instant::now(), shared: shared2 };
        let _ = ready.set(c.clone());
        say(&c.ui, "Attached");
        tokio::spawn(redraw_events(c.clone()));
        tokio::spawn(events(c.clone()));
        tokio::spawn(follow(c.clone(), DEVICES));
        tokio::spawn(follow(c.clone(), STATUS));
        if let Ok(v) = ask(&c, DEVICES).await {
            show_devices(&c, &v["devices"]);
        }
        profiles(&c).await;
    });
    shared
}

/// Once the window has closed: the streams this panel started are stopped.
pub async fn finish(shared: Arc<Shared>) {
    let Some(at) = shared.attachment.get() else { return };
    let started = std::mem::take(&mut *shared.started.lock().unwrap());
    for stream in started {
        let _ = tokio::time::timeout(Duration::from_secs(2), at.stop_stream(&shared.unit, &stream)).await;
    }
}

async fn command(c: &Ctx, r#type: &str, payload: Value) -> Result<String, String> {
    match c.at.command(&c.unit, r#type, payload.to_string().as_bytes()).await {
        Ok(ack) => match ack.outcome {
            2 => Ok(ack.line),
            1 => Err(format!("still running — {}", ack.line)),
            _ => Err(ack.line),
        },
        Err(e) => Err(e.to_string()),
    }
}

async fn ask(c: &Ctx, r#type: &str) -> Result<Value, String> {
    let answer = c.at.ask(&c.unit, r#type, b"").await.map_err(|e| e.to_string())?;
    serde_json::from_slice(&answer).map_err(|e| e.to_string())
}

fn log(c: &Ctx, line: String) {
    let mut l = c.log.lock().unwrap();
    l.lines.push_front(format!("+{:>8.3}s  {line}", c.t0.elapsed().as_secs_f32()));
    l.lines.truncate(MAX_EVENTS);
    l.dirty = true;
}

/// The profiles this host offers. A host with no profile daemon says so instead.
async fn profiles(c: &Ctx) {
    let (list, note) = match ask(c, PROFILES).await {
        Ok(v) => {
            let list: Vec<SharedString> =
                v["available"].as_array().into_iter().flatten().filter_map(Value::as_str).map(SharedString::from).collect();
            let degraded = s(&v["degraded"]);
            let note = if degraded.is_empty() { String::new() } else { format!("performance held back: {degraded}") };
            (list, note)
        }
        // The Core says whose refusal it carries; the window needs only the reason.
        Err(e) => (Vec::new(), e.strip_prefix("unit.failed: ").unwrap_or(&e).to_string()),
    };
    let _ = c.ui.upgrade_in_event_loop(move |u| {
        u.set_profiles(ModelRc::new(VecModel::from(list)));
        u.set_profiles_note(note.into());
    });
}

async fn follow(c: Ctx, stream: &'static str) {
    loop {
        match c.at.subscribe_stream(&c.unit, stream).await {
            Ok(mut delivery) => {
                if !delivery.flowing {
                    match c.at.start_stream(&c.unit, stream).await {
                        Ok(_) => c.shared.started.lock().unwrap().push(stream.to_string()),
                        Err(e) => log(&c, format!("{stream} cannot be started: {e}")),
                    }
                }
                let mut seen = 0u64;
                if stream == STATUS {
                    match ask(&c, STATUS).await {
                        Ok(v) => {
                            seen = v["sequence"].as_u64().unwrap_or(0);
                            on_status(&c, &v);
                        }
                        Err(e) => say(&c.ui, format!("{STATUS} cannot be asked: {e}")),
                    }
                }
                loop {
                    match delivery.next().await {
                        Ok(Some(frame)) => {
                            let Ok(v) = serde_json::from_slice::<Value>(&frame.payload) else { continue };
                            if stream == DEVICES {
                                show_devices(&c, &v["devices"]);
                            } else {
                                let sequence = v["sequence"].as_u64().unwrap_or(0);
                                if sequence > seen {
                                    seen = sequence;
                                    on_status(&c, &v);
                                }
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            log(&c, format!("{stream}: {e}"));
                            break;
                        }
                    }
                }
                log(&c, format!("{stream}: the delivery ended; subscribing again"));
            }
            Err(e) => log(&c, format!("{stream} cannot be subscribed to: {e}")),
        }
        sleep(AGAIN).await;
    }
}

fn on_status(c: &Ctx, v: &Value) {
    let on_battery = v["on_battery"].as_bool().unwrap_or(false);
    let b = &v["battery"];
    let has_battery = !b.is_null();
    let pct = b["percentage"].as_f64().unwrap_or(0.0);
    let state = s(&b["state"]).replace('-', " ");
    let left = if on_battery {
        duration(b["time_to_empty"].as_i64().unwrap_or(0)).map(|d| format!("{d} left"))
    } else {
        duration(b["time_to_full"].as_i64().unwrap_or(0)).map(|d| format!("full in {d}"))
    };
    let warning = s(&b["warning_level"]);
    let profile = s(&v["profile"]);
    log(c, format!("status   #{} {} {pct:.0}% {state}", v["sequence"], if on_battery { "battery" } else { "external" }));
    let _ = c.ui.upgrade_in_event_loop(move |u| {
        u.set_on_battery(on_battery);
        u.set_has_battery(has_battery);
        u.set_percent(pct.round() as i32);
        u.set_percentage(format!("{pct:.0}").into());
        u.set_state(state.into());
        u.set_time_left(left.unwrap_or_default().into());
        u.set_warning(warning.into());
        u.set_profile(profile.into());
        if u.get_message() == "Attached" || u.get_message() == "Starting..." {
            u.set_message("Ready".into());
        }
    });
}

/// The batteries first, then the rest; a supply shows whether it is plugged in.
fn show_devices(c: &Ctx, devices: &Value) {
    let mut list = devices.as_array().cloned().unwrap_or_default();
    list.sort_by_key(|d| match d["kind"].as_str() {
        Some("battery") => 0,
        Some("line-power") => 1,
        _ => 2,
    });
    let rows: Vec<DeviceRow> = list
        .iter()
        .map(|d| {
            let kind = s(&d["kind"]);
            let supply = kind == "line-power";
            let name = [s(&d["vendor"]), s(&d["model"])].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" ");
            let name = match (name.is_empty(), supply) {
                (false, _) => name,
                (true, true) => "AC adapter".to_string(),
                (true, false) => s(&d["id"]),
            };
            let pct = d["percentage"].as_f64().unwrap_or(0.0);
            let shows = !supply && d["present"].as_bool().unwrap_or(false);
            let rate = d["energy_rate"].as_f64().unwrap_or(0.0);
            let detail = if supply {
                if d["online"].as_bool().unwrap_or(false) { "plugged in".to_string() } else { "unplugged".to_string() }
            } else if rate > 0.0 {
                format!("{rate:.1} W")
            } else {
                String::new()
            };
            DeviceRow {
                id: s(&d["id"]).into(),
                kind: kind.replace('-', " ").into(),
                name: name.into(),
                percentage: if shows { format!("{pct:.0}") } else { String::new() }.into(),
                percent: pct.round() as i32,
                state: if supply { String::new() } else { s(&d["state"]).replace('-', " ") }.into(),
                detail: detail.into(),
            }
        })
        .collect();
    let _ = c.ui.upgrade_in_event_loop(move |u| u.set_devices(ModelRc::new(VecModel::from(rows))));
}

async fn events(c: Ctx) {
    let filter = Filter { subject_kind: "unit".into(), subject_identity: c.unit.clone(), ..Default::default() };
    let mut sub = match c.at.subscribe(filter).await {
        Ok(s) => s,
        Err(e) => {
            log(&c, format!("events cannot be subscribed to: {e}"));
            return;
        }
    };
    loop {
        match sub.next().await {
            Ok(Some(sub)) => match sub.carries {
                Some(Carries::Event(e)) => {
                    let detail: Value = serde_json::from_slice(&e.detail).unwrap_or(Value::Null);
                    let line = detail["line"].as_str().map(str::to_string).unwrap_or_else(|| {
                        let d = detail.to_string();
                        if d.len() > 160 { format!("{}…", &d[..160]) } else { d }
                    });
                    let class = if e.occurrence.is_empty() { String::new() } else { format!(" {}", e.occurrence) };
                    log(&c, format!("event    {} {}{class} [{}] {line}", e.seq, e.r#type, e.severity));
                    // What the person should see at once: the supply changing, the battery running low.
                    if e.occurrence.starts_with("power.") {
                        say(&c.ui, line);
                    }
                }
                Some(Carries::Overflow(_)) => log(&c, "events: the panel fell behind and some were dropped".into()),
                _ => {}
            },
            Ok(None) => break,
            Err(e) => {
                log(&c, format!("events: {e}"));
                break;
            }
        }
    }
}

async fn redraw_events(c: Ctx) {
    let mut tick = interval(EVENTS_REDRAW);
    loop {
        tick.tick().await;
        let rows = {
            let mut l = c.log.lock().unwrap();
            if !l.dirty {
                continue;
            }
            l.dirty = false;
            l.lines.iter().map(|s| SharedString::from(s.as_str())).collect::<Vec<_>>()
        };
        let _ = c.ui.upgrade_in_event_loop(move |u| u.set_events(ModelRc::new(VecModel::from(rows))));
    }
}
