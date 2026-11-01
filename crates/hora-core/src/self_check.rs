//! Hora watching itself: the built-in monitor [`SELF_MONITOR_ID`].
//!
//! The dead-man heartbeat catches a dead process, a wedged scheduler and a
//! broken database. What it cannot see is subtler: one monitor that silently
//! stopped checking, a monitor task stuck in a panic/restart loop, a
//! notification channel that keeps failing, a config edit the file watcher
//! never announced. Every minute this task looks for
//! those and reports them through the self-check monitor (down while any is
//! present), so they alert like an outage.
//!
//! When no alert can reach anybody - every channel failing, or the alert
//! coalescer gone - an alert from Hora is pointless: [`SelfHealth::cannot_deliver`]
//! makes the heartbeat stop, and the dead-man service raises the alarm.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{error, warn};

use crate::config::{Config, MonitorKind, SELF_MONITOR_ID, is_builtin_monitor};
use crate::config_watch::{ConfigWatch, fingerprint};
use crate::db::{self, Store};
use crate::notifications::{self, Notifiers};
use crate::status::CheckStatus;

/// How often the self-check looks.
const CHECK_EVERY: Duration = Duration::from_mins(1);
/// Re-record an unchanged state this often (the monitor's deadline is far longer).
const REFRESH: Duration = Duration::from_hours(6);
/// A task death stays reported this long after it happened.
const PANIC_WINDOW_SECS: i64 = 3600;
/// A monitor is stale after this many of its periods without a check.
const STALE_PERIODS: u64 = 3;
/// Slack on top, for probe timeouts and scheduling jitter.
const STALE_SLACK_SECS: u64 = 60;
/// The stored message is bounded; the log has the details.
const MAX_MESSAGE_CHARS: usize = 1000;

/// What the supervisor tells the self-check (and the heartbeat reads).
#[derive(Debug, Default)]
pub struct SelfHealth {
    deaths: Mutex<VecDeque<Death>>,
    coalescer_dead: AtomicBool,
    channels_down: AtomicBool,
}

#[derive(Debug, Clone)]
struct Death {
    at: i64,
    what: String,
    id: String,
}

impl SelfHealth {
    /// A monitor or peer-watch task stopped unexpectedly (and was restarted).
    pub fn task_died(&self, what: &str, id: &str, at: i64) {
        let mut deaths = self.deaths.lock().unwrap_or_else(PoisonError::into_inner);
        deaths.push_back(Death {
            at,
            what: what.to_owned(),
            id: id.to_owned(),
        });
        while deaths.len() > 100 {
            deaths.pop_front();
        }
    }

    /// The alert coalescer stopped: no down/recovered alert is delivered any more.
    pub fn coalescer_died(&self) {
        self.coalescer_dead.store(true, Ordering::Release);
    }

    /// No alert can reach anybody: every channel is failing, or the coalescer is gone.
    #[must_use]
    pub fn cannot_deliver(&self) -> bool {
        self.coalescer_dead.load(Ordering::Acquire) || self.channels_down.load(Ordering::Acquire)
    }

    fn recent_deaths(&self, now: i64) -> Vec<Death> {
        let mut deaths = self.deaths.lock().unwrap_or_else(PoisonError::into_inner);
        deaths.retain(|death| now - death.at <= PANIC_WINDOW_SECS);
        deaths.iter().cloned().collect()
    }
}

/// One channel's delivery state, as the self-check judges it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelState {
    pub name: String,
    pub consecutive_failures: u32,
}

/// Everything wrong right now. Pure, for the tests.
#[must_use]
pub(crate) fn findings(
    config: &Config,
    last_checks: &HashMap<String, i64>,
    first_seen: &HashMap<String, i64>,
    channels: &[ChannelState],
    deaths: &[(String, String)],
    coalescer_dead: bool,
    now: i64,
) -> Vec<String> {
    let mut found = Vec::new();
    if coalescer_dead {
        found.push("the alert coalescer stopped: down and recovered alerts are not delivered until restart".to_owned());
    }
    let threshold = config.alerts.channel_fail_threshold.max(1);
    let failing: Vec<&ChannelState> = channels
        .iter()
        .filter(|c| c.consecutive_failures >= threshold)
        .collect();
    for channel in &failing {
        found.push(format!(
            "channel {} failed {} deliveries in a row",
            channel.name, channel.consecutive_failures
        ));
    }
    let mut died: Vec<String> = deaths
        .iter()
        .map(|(what, id)| format!("{what} {id}"))
        .collect();
    died.sort();
    died.dedup();
    for task in died {
        found.push(format!(
            "{task} task crashed within the last hour (restarted)"
        ));
    }
    for monitor in &config.monitors {
        // Push monitors watch their own deadline; built-ins are fed by Hora.
        if matches!(monitor.spec, MonitorKind::Push(_)) || is_builtin_monitor(&monitor.id) {
            continue;
        }
        let period = monitor.interval_secs.max(monitor.timeout_secs);
        let allowed = i64::try_from(period.saturating_mul(STALE_PERIODS) + STALE_SLACK_SECS)
            .unwrap_or(i64::MAX);
        let since = last_checks
            .get(&monitor.id)
            .copied()
            .max(first_seen.get(&monitor.id).copied())
            .unwrap_or(now);
        if now - since > allowed {
            found.push(format!(
                "monitor {} has not checked for {} min (interval {} s)",
                monitor.id,
                (now - since) / 60,
                monitor.interval_secs
            ));
        }
    }
    found
}

/// Start the self-check until `shutdown`.
#[must_use]
pub fn spawn(
    store: Store,
    mut config: watch::Receiver<Arc<Config>>,
    notifier: Notifiers,
    health: Arc<SelfHealth>,
    config_watch: Arc<ConfigWatch>,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut first_seen: HashMap<String, i64> = HashMap::new();
        let mut drift = Drift::default();
        let mut recorded: Option<(CheckStatus, String, i64)> = None;
        loop {
            let snapshot = config.borrow_and_update().clone();
            let now = chrono::Utc::now().timestamp();
            first_seen.retain(|id, _| snapshot.find_monitor(id).is_some());
            for monitor in &snapshot.monitors {
                first_seen.entry(monitor.id.clone()).or_insert(now);
            }
            let channels: Vec<ChannelState> = notifications::health_snapshot(&notifier)
                .into_iter()
                .map(|entry| ChannelState {
                    name: entry.name,
                    consecutive_failures: entry.health.consecutive_failures,
                })
                .collect();
            let threshold = snapshot.alerts.channel_fail_threshold.max(1);
            let all_down = !channels.is_empty()
                && channels.iter().all(|c| c.consecutive_failures >= threshold);
            if all_down != health.channels_down.swap(all_down, Ordering::AcqRel) && all_down {
                error!(
                    "every notification channel is failing: Hora withholds its external heartbeat"
                );
            }
            let last_checks = match db::last_check_times(&store).await {
                Ok(times) => times,
                Err(err) => {
                    warn!("self-check cannot read the last checks: {err:#}");
                    HashMap::new()
                }
            };
            let deaths: Vec<(String, String)> = health
                .recent_deaths(now)
                .into_iter()
                .map(|death| (death.what, death.id))
                .collect();
            let mut found = findings(
                &snapshot,
                &last_checks,
                &first_seen,
                &channels,
                &deaths,
                health.coalescer_dead.load(Ordering::Acquire),
                now,
            );
            if let Some(finding) = drift.check(&config_watch, now) {
                found.push(finding);
            }
            let (status, message) = state(&found);
            let changed = recorded.as_ref().is_none_or(|(s, m, at)| {
                *s != status || *m != message || now - at >= REFRESH.as_secs().cast_signed()
            });
            if changed {
                match db::insert_push(&store, SELF_MONITOR_ID, status, None, Some(&message)).await {
                    Ok(()) => recorded = Some((status, message, now)),
                    Err(err) => warn!("cannot record the self-check: {err:#}"),
                }
            }
            tokio::select! {
                () = tokio::time::sleep(CHECK_EVERY) => {}
                _ = shutdown.changed() => break,
            }
        }
    })
}

/// Notices a config edit that no file event announced: the file on disk differs
/// from the text the supervisor applied, unchanged across two checks (a real
/// event would have reloaded it within a second). Hora then reloads anyway, and
/// reports the miss for [`PANIC_WINDOW_SECS`]: the watcher is not to be trusted.
#[derive(Debug, Default)]
struct Drift {
    /// The differing text seen at the previous check.
    pending: Option<u64>,
    /// When a change was last missed.
    missed_at: Option<i64>,
}

impl Drift {
    fn check(&mut self, watch: &ConfigWatch, now: i64) -> Option<String> {
        match std::fs::read_to_string(watch.path()) {
            // The supervisor reports an unreadable file through the config monitor.
            Err(_) => {
                self.pending = None;
                watch.request_reload();
            }
            Ok(raw) if watch.is_applied(&raw) => self.pending = None,
            Ok(raw) => {
                let seen = fingerprint(&raw);
                if self.pending == Some(seen) {
                    warn!("the config file changed without a file event; reloading it now");
                    self.missed_at = Some(now);
                    self.pending = None;
                    watch.request_reload();
                } else {
                    self.pending = Some(seen);
                }
            }
        }
        let missed_at = self.missed_at.filter(|at| now - at <= PANIC_WINDOW_SECS)?;
        Some(format!(
            "the config file changed without a file event {} min ago (applied by the \
             self-check); live reload is not reliable here",
            (now - missed_at) / 60
        ))
    }
}

/// The self-check monitor's status and message for `found`.
#[must_use]
pub fn state(found: &[String]) -> (CheckStatus, String) {
    if found.is_empty() {
        return (
            CheckStatus::Up,
            "all monitors checking, channels delivering".to_owned(),
        );
    }
    let message = format!(
        "{} self-check finding(s): {}",
        found.len(),
        found.join("; ")
    );
    if message.chars().count() > MAX_MESSAGE_CHARS {
        let cut: String = message.chars().take(MAX_MESSAGE_CHARS - 1).collect();
        return (CheckStatus::Down, format!("{cut}…"));
    }
    (CheckStatus::Down, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn config() -> Config {
        crate::config::parse_lenient_with_exec_dir(
            r#"
            [page]
            [server]
            [alerts]
            channel_fail_threshold = 3
            [[monitors]]
            id = "web"
            name = "Web"
            target = "https://example.com"
            interval_secs = 60
            [[monitors]]
            id = "job"
            name = "Job"
            kind = "push"
            interval_secs = 60
            push_token = "0123456789abcdef"
            "#,
            None,
        )
        .unwrap()
    }

    fn run(
        last: &[(&str, i64)],
        seen: i64,
        channels: &[(&str, u32)],
        deaths: &[(&str, &str)],
        coalescer: bool,
    ) -> Vec<String> {
        let config = config();
        let last: HashMap<String, i64> =
            last.iter().map(|(id, t)| ((*id).to_owned(), *t)).collect();
        let first_seen: HashMap<String, i64> = config
            .monitors
            .iter()
            .map(|m| (m.id.clone(), seen))
            .collect();
        let channels: Vec<ChannelState> = channels
            .iter()
            .map(|(name, failures)| ChannelState {
                name: (*name).to_owned(),
                consecutive_failures: *failures,
            })
            .collect();
        let deaths: Vec<(String, String)> = deaths
            .iter()
            .map(|(w, id)| ((*w).to_owned(), (*id).to_owned()))
            .collect();
        findings(
            &config,
            &last,
            &first_seen,
            &channels,
            &deaths,
            coalescer,
            NOW,
        )
    }

    #[test]
    fn a_healthy_node_has_no_findings() {
        let found = run(&[("web", NOW - 30)], NOW - 3600, &[("push", 0)], &[], false);
        assert_eq!(found, Vec::<String>::new());
        assert_eq!(state(&found).0, CheckStatus::Up);
    }

    #[test]
    fn a_monitor_that_stopped_checking_is_reported() {
        // 60 s interval: allowed 3 x 60 + 60 = 240 s.
        let found = run(&[("web", NOW - 600)], NOW - 3600, &[], &[], false);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].starts_with("monitor web has not checked for 10 min"),
            "{found:?}"
        );
        assert_eq!(state(&found).0, CheckStatus::Down);
    }

    #[test]
    fn a_new_monitor_gets_its_grace_and_push_and_builtins_are_skipped() {
        // No check yet, but only seen 100 s ago: not stale. `job` (push) and the
        // built-ins never count, however long they are silent.
        let found = run(&[], NOW - 100, &[], &[], false);
        assert_eq!(found, Vec::<String>::new());
        let found = run(&[], NOW - 3600, &[], &[], false);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].starts_with("monitor web "), "{found:?}");
    }

    #[test]
    fn failing_channels_crashed_tasks_and_a_dead_coalescer_are_reported() {
        let found = run(
            &[("web", NOW - 30)],
            NOW - 3600,
            &[("ok", 2), ("pushover", 3)],
            &[("monitor", "web"), ("monitor", "web")],
            true,
        );
        assert_eq!(found.len(), 3, "{found:?}");
        assert!(found[0].contains("coalescer"));
        assert_eq!(found[1], "channel pushover failed 3 deliveries in a row");
        assert_eq!(
            found[2],
            "monitor web task crashed within the last hour (restarted)"
        );
    }

    #[test]
    fn deaths_age_out_and_a_dead_coalescer_blocks_delivery() {
        let health = SelfHealth::default();
        health.task_died("monitor", "web", NOW - PANIC_WINDOW_SECS - 1);
        health.task_died("monitor", "api", NOW - 10);
        let recent: Vec<String> = health
            .recent_deaths(NOW)
            .into_iter()
            .map(|d| d.id)
            .collect();
        assert_eq!(recent, ["api"]);
        assert!(!health.cannot_deliver());
        health.coalescer_died();
        assert!(health.cannot_deliver());
    }

    #[test]
    fn a_change_without_a_file_event_is_applied_and_reported() {
        let dir = std::env::temp_dir().join(format!("hora-drift-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "one").unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let watch = ConfigWatch::for_test(path.clone(), tx);
        watch.applied("one");
        let mut drift = Drift::default();
        assert_eq!(drift.check(&watch, 0), None, "in sync");

        std::fs::write(&path, "two").unwrap();
        assert_eq!(
            drift.check(&watch, 60),
            None,
            "a file event may still be on its way"
        );
        assert!(rx.try_recv().is_err());
        let finding = drift.check(&watch, 120).expect("missed on the second look");
        assert!(
            finding.contains("without a file event 0 min ago"),
            "{finding}"
        );
        assert!(rx.try_recv().is_ok(), "the self-check asks for the reload");

        // Applied now: the miss stays reported for an hour, then ages out.
        watch.applied("two");
        assert!(drift.check(&watch, 180).is_some());
        assert_eq!(drift.check(&watch, 120 + PANIC_WINDOW_SECS + 1), None);

        // A change a file event applied in time is never a finding.
        let mut drift = Drift::default();
        std::fs::write(&path, "three").unwrap();
        assert_eq!(drift.check(&watch, 0), None);
        watch.applied("three");
        assert_eq!(drift.check(&watch, 60), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
