//! Feeds the built-in configuration monitor ([`CONFIG_MONITOR_ID`]): at every
//! configuration change, and every few hours so its heartbeat never lapses, it
//! records the load's state as a push - down while entries were dropped or the
//! file could not be loaded, up otherwise. Alerting, reminders, recovery and
//! the status page then treat it like any other monitor.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::warn;

use crate::config::{CONFIG_MONITOR_ID, Config};
use crate::db::{self, Store};
use crate::status::CheckStatus;

/// Re-record the unchanged state this often (the monitor's deadline is far longer).
const REFRESH: Duration = Duration::from_hours(6);
/// The stored message is bounded; the log has every issue in full.
const MAX_MESSAGE_CHARS: usize = 1000;

/// Start feeding the configuration monitor until `shutdown`.
#[must_use]
pub fn spawn(
    store: Store,
    mut config: watch::Receiver<Arc<Config>>,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let snapshot = config.borrow_and_update().clone();
            record(&store, &snapshot).await;
            tokio::select! {
                changed = config.changed() => if changed.is_err() { break },
                () = tokio::time::sleep(REFRESH) => {}
                _ = shutdown.changed() => break,
            }
        }
    })
}

/// The status and message the configuration monitor shows for `config`.
#[must_use]
pub fn state(config: &Config) -> (CheckStatus, String) {
    if config.issues.is_empty() {
        return (
            CheckStatus::Up,
            "configuration loaded without issues".to_owned(),
        );
    }
    let list = config
        .issues
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    let message = format!("{} configuration issue(s): {list}", config.issues.len());
    let message = if message.chars().count() > MAX_MESSAGE_CHARS {
        let cut: String = message.chars().take(MAX_MESSAGE_CHARS - 1).collect();
        format!("{cut}…")
    } else {
        message
    };
    (CheckStatus::Down, message)
}

async fn record(store: &Store, config: &Config) {
    let (status, message) = state(config);
    if let Err(err) = db::insert_push(store, CONFIG_MONITOR_ID, status, None, Some(&message)).await
    {
        warn!("cannot record the configuration monitor's state: {err:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issues_turn_the_config_monitor_down_with_every_issue_named() {
        let clean = crate::config::parse_lenient_with_exec_dir("[page]\n[server]\n", None).unwrap();
        assert_eq!(state(&clean).0, CheckStatus::Up);
        let faulty = crate::config::parse_lenient_with_exec_dir(
            "[page]\n[server]\n[[monitors]]\nid = \"bad\"\nname = \"B\"\ntarget = \"https://e.com\"\ninterval_secs = 0\n",
            None,
        )
        .unwrap();
        let (status, message) = state(&faulty);
        assert_eq!(status, CheckStatus::Down);
        assert!(
            message.starts_with("1 configuration issue(s): monitor bad:"),
            "{message}"
        );
    }
}
