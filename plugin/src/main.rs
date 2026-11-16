//! power: the host's supply, batteries and power profile as a Yoke Plugin unit.

use std::sync::Arc;
use std::time::Duration;

use power::backend::{PowerBackend, Upower};
use power::plugin::{declaration, handle, Power};
use tokio::sync::broadcast::error::RecvError;
use yoke_sdk::plugin::{start, Event};

/// UPower says a battery changed many times a second while it charges; the Plugin reads again at most
/// this often.
const SETTLE: Duration = Duration::from_millis(500);

#[tokio::main]
async fn main() {
    // `power --manifest` prints the Manifest the declaration generates, which the bundle carries.
    if std::env::args().nth(1).as_deref() == Some("--manifest") {
        print!("{}", declaration().manifest());
        return;
    }
    let unit = match start(&declaration()).await {
        Ok(unit) => Arc::new(unit),
        Err(err) => {
            eprintln!("power: {err}");
            std::process::exit(1);
        }
    };
    let backend = Upower::connect().await.map(|b| b as Arc<dyn PowerBackend>);
    let (grade, line) = match &backend {
        Ok(_) => (0, "UPower connected".to_string()),
        Err(e) => (50, format!("no UPower: {e}")),
    };
    if let Err(err) = unit.health(grade, &line).await {
        eprintln!("power: {err}");
    }
    let mut said = backend.as_ref().ok().map(|b| b.subscribe());
    let power = Power::new(backend);
    if let Err(err) = power.refresh(unit.as_ref()).await {
        eprintln!("power: {err}");
    }
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("power listens for SIGTERM");
    loop {
        tokio::select! {
            event = unit.next() => match event {
                None | Some(Event::Ended(_)) => return,
                Some(Event::Activated(a)) => {
                    if let Err(err) = power.activated(unit.as_ref(), &a.stream).await {
                        eprintln!("power: {err}");
                    }
                }
                Some(Event::Stopped { stream }) => power.stopped(&stream),
                Some(event) => {
                    let (unit, power) = (unit.clone(), power.clone());
                    tokio::spawn(async move {
                        if let Err(err) = handle(unit.as_ref(), &event, &power).await {
                            eprintln!("power: {err}");
                        }
                    });
                }
            },
            ev = async { match said.as_mut() { Some(rx) => rx.recv().await, None => std::future::pending().await } } => match ev {
                Ok(()) | Err(RecvError::Lagged(_)) => {
                    // Whatever else arrives meanwhile is the same change.
                    tokio::time::sleep(SETTLE).await;
                    if let Some(rx) = said.as_mut() {
                        while !matches!(rx.try_recv(), Err(tokio::sync::broadcast::error::TryRecvError::Empty | tokio::sync::broadcast::error::TryRecvError::Closed)) {}
                    }
                    if let Err(err) = power.refresh(unit.as_ref()).await {
                        eprintln!("power: {err}");
                    }
                }
                Err(RecvError::Closed) => {
                    said = None;
                    let _ = unit.health(50, "UPower went away").await;
                }
            },
            _ = term.recv() => {
                let _ = unit.close().await;
                while let Some(event) = unit.next().await {
                    if matches!(event, Event::Ended(_)) {
                        break;
                    }
                }
                return;
            }
        }
    }
}
