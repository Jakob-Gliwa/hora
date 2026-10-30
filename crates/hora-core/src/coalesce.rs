//! Root-cause alert grouping: one notification per incident, not one per
//! affected monitor.
//!
//! Every down/recovered alert flows through here. Alerts from monitors with
//! no configured upstreams (`depends_on`) are roots: they go out immediately,
//! and their `impacted` list already names the blast radius. Monitors *with*
//! upstreams wait out a short grouping window: if any transitive upstream
//! alerted - before, or while they wait - the alert folds into that single
//! notification (suppressed), and so does its eventual recovery. Grouping is
//! keyed on the *configured* upstreams, not on the cause derived at
//! confirmation time: in a cascade the dependent often confirms a tick before
//! its upstream has enough recorded failures to be derivably down, and the
//! grouping must not lose that race. A monitor that recovers while still
//! queued was a flap inside the window: nothing is sent at all. Incident
//! *records* are written by the scheduler before alerts reach this point, so
//! the history stays complete whatever is folded here.
//!
//! A *local-only* down (every peer that answered sees the target up) is not an
//! outage: it skips the grouping, covers nothing, and goes to the
//! `alerts.notify_unconfirmed` channels instead of the monitor's own (or
//! nowhere without them). If the peers later confirm it, the real down follows
//! as usual. A recovery goes to whoever received a down: the usual channels,
//! the quiet ones, or both.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use hora_notify::Event;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::info;

use crate::config::Config;
use crate::notifications::Notifiers;

/// One alert routed through the coalescer.
#[derive(Debug)]
pub enum AlertMsg {
    Down(DownAlert),
    Repeat(DownAlert),
    Recovered {
        id: String,
        name: String,
        notify: Option<Vec<String>>,
    },
    /// A monitor task resumed an open incident whose down was local-only
    /// (from its `local_only` column): the routing a restart lost, so the
    /// recovery still goes only to whoever received the down. `confirmed`:
    /// the peers later confirmed it and the usual down went out too.
    ResumedLocalOnly {
        id: String,
        confirmed: bool,
    },
}

/// A confirmed-down alert, with its topology annotations resolved.
#[derive(Debug)]
pub struct DownAlert {
    pub id: String,
    pub name: String,
    pub error: Option<String>,
    /// Every transitive upstream id from the config: the grouping key.
    pub upstreams: Vec<String>,
    /// The nearest *derived-down* upstream's display name, for the message.
    pub cause_name: Option<String>,
    pub impacted: Vec<String>,
    pub notify: Option<Vec<String>>,
    /// Multi-vantage verdict, when peers were asked before this alert.
    pub vantage: Option<String>,
    /// Correlated event marker, when one was recorded shortly before the down
    /// ("deploy api v2.3, 3m before").
    pub event: Option<String>,
    /// Down from this node only (see [`crate::mesh::confirm::DownVerdict`]).
    pub local_only: bool,
}

/// Who receives a recovery: whoever received the down it ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recipients {
    /// Nobody: the down was folded, dropped as a flap, or local-only with no
    /// quiet channel configured.
    Nobody,
    /// The monitor's own channels.
    Usual,
    /// Only the `notify_unconfirmed` channels: the down was local-only.
    Quiet,
    /// Both: a local-only down later confirmed by the peers.
    Both,
}

/// A local-only down announced (to the quiet channels, if any).
#[derive(Debug, Clone, Copy, Default)]
struct Quiet {
    /// The peers later confirmed it: the usual down went out too.
    confirmed: bool,
}

/// How long a sent (or folded) down alert keeps absorbing late symptom
/// confirmations: monitors confirm at their own interval × threshold pace, so
/// a cascade can take minutes to fully declare itself.
const CAUSE_MEMORY_SECS: i64 = 600;

/// Sweep cadence with an empty queue; the driver wakes earlier for deadlines.
const IDLE_SWEEP: Duration = Duration::from_hours(1);

/// The grouping state machine, pure (time comes in as unix seconds) so the
/// decision table is unit-testable without a runtime.
#[derive(Default)]
struct Coalescer {
    /// Symptom alerts waiting out the grouping window.
    pending: Vec<Pending>,
    /// Down alerts recently sent or folded (id → when): anything caused by
    /// them folds too while the memory is fresh.
    covered: HashMap<String, i64>,
    /// Monitors whose down alert was folded away: their recovery is too.
    suppressed: HashSet<String>,
    /// Monitors whose current down was announced as local-only.
    quiet: HashMap<String, Quiet>,
}

struct Pending {
    deadline: i64,
    /// A symptom may wait once more for a cause that is itself still queued.
    requeued: bool,
    alert: DownAlert,
}

impl Coalescer {
    /// Route a confirmed-down alert. `Some` = send it now.
    fn on_down(&mut self, alert: DownAlert, now: i64, window_secs: u64) -> Option<DownAlert> {
        self.prune(now);
        if alert.local_only {
            // Not an outage: no grouping wait, and it covers nothing - a
            // dependent's real down must not fold into it.
            self.quiet.insert(alert.id.clone(), Quiet::default());
            return Some(alert);
        }
        if let Some(quiet) = self.quiet.get_mut(&alert.id) {
            quiet.confirmed = true;
        }
        if alert.upstreams.is_empty() || window_secs == 0 {
            // A root (nothing above it in the topology), or grouping disabled:
            // send immediately. The impacted list already names the dependents
            // being folded.
            self.mark_sent(&alert.id, now);
            return Some(alert);
        }
        if self.covered_upstream(&alert) {
            self.fold(alert, now);
            return None;
        }
        self.pending.push(Pending {
            deadline: now.saturating_add(i64::try_from(window_secs).unwrap_or(i64::MAX)),
            requeued: false,
            alert,
        });
        None
    }

    /// Whether any of the alert's transitive upstreams recently alerted (or
    /// folded), i.e. this alert is already covered by an incident.
    fn covered_upstream(&self, alert: &DownAlert) -> bool {
        alert
            .upstreams
            .iter()
            .any(|upstream| self.covered.contains_key(upstream.as_str()))
    }

    fn can_repeat(&self, id: &str) -> bool {
        !self.suppressed.contains(id)
            && !self.pending.iter().any(|p| p.alert.id == id)
            && self.quiet.get(id).is_none_or(|q| q.confirmed)
    }

    /// Route a recovery to whoever received the down: folded downs recover
    /// silently, a down still queued is dropped outright (a flap inside the
    /// window), and a local-only down recovers on the quiet channels only.
    fn on_recovered(&mut self, id: &str, now: i64) -> Recipients {
        self.prune(now);
        self.covered.remove(id);
        let quiet = self.quiet.remove(id);
        let usual = if let Some(position) = self.pending.iter().position(|p| p.alert.id == id) {
            self.pending.swap_remove(position);
            false
        } else {
            !self.suppressed.remove(id) && quiet.is_none_or(|quiet| quiet.confirmed)
        };
        match (usual, quiet.is_some()) {
            (true, true) => Recipients::Both,
            (true, false) => Recipients::Usual,
            (false, true) => Recipients::Quiet,
            (false, false) => Recipients::Nobody,
        }
    }

    /// Release every expired alert: folded if an upstream alerted meanwhile,
    /// sent otherwise. An expired alert with an upstream *itself still queued*
    /// waits (once) for that upstream's own verdict.
    fn flush(&mut self, now: i64) -> Vec<DownAlert> {
        self.prune(now);
        let mut out = Vec::new();
        loop {
            // Deadline order, so a queued upstream resolves before dependents.
            let expired = self
                .pending
                .iter()
                .enumerate()
                .filter(|(_, p)| p.deadline <= now)
                .min_by_key(|(_, p)| p.deadline)
                .map(|(index, _)| index);
            let Some(index) = expired else { break };
            let mut pending = self.pending.swap_remove(index);

            let upstream_queued = self
                .pending
                .iter()
                .filter(|p| pending.alert.upstreams.contains(&p.alert.id))
                .map(|p| p.deadline)
                .max();
            if self.covered_upstream(&pending.alert) {
                self.fold(pending.alert, now);
            } else if let Some(upstream_deadline) = upstream_queued
                && !pending.requeued
            {
                pending.requeued = true;
                pending.deadline = upstream_deadline.max(now).saturating_add(1);
                self.pending.push(pending);
            } else {
                self.mark_sent(&pending.alert.id, now);
                out.push(pending.alert);
            }
        }
        out
    }

    /// Empty the queue on shutdown: fold what an alerted upstream covers, send
    /// the rest - dropping confirmed downs on the floor is never right.
    fn drain(&mut self, now: i64) -> Vec<DownAlert> {
        self.prune(now);
        let pending = std::mem::take(&mut self.pending);
        pending
            .into_iter()
            .filter_map(|p| {
                if self.covered_upstream(&p.alert) {
                    self.fold(p.alert, now);
                    None
                } else {
                    self.suppressed.remove(&p.alert.id);
                    Some(p.alert)
                }
            })
            .collect()
    }

    /// A down alert goes out: it covers its dependents, and its recovery must
    /// be announced - even if an earlier, folded down of the same monitor left
    /// it in `suppressed` (a recovery that never reached us, a restarted task).
    fn mark_sent(&mut self, id: &str, now: i64) {
        self.covered.insert(id.to_owned(), now);
        self.suppressed.remove(id);
    }

    /// Forget folded downs of monitors no longer configured: their recovery
    /// will never come, and a monitor re-added under the same id later must
    /// not have its first real recovery swallowed.
    /// Seed the routing of a local-only down announced before a restart.
    fn resume_local_only(&mut self, id: String, confirmed: bool) {
        self.quiet.insert(id, Quiet { confirmed });
    }

    fn retain_monitors(&mut self, known: impl Fn(&str) -> bool) {
        self.suppressed.retain(|id| known(id));
        self.quiet.retain(|id, _| known(id));
    }

    /// Fold a symptom into its (already alerted) cause. The symptom becomes a
    /// cover itself, so a whole dependency chain collapses into one alert.
    fn fold(&mut self, alert: DownAlert, now: i64) {
        info!(monitor = %alert.id, "down alert folded into its root cause");
        self.covered.insert(alert.id.clone(), now);
        self.suppressed.insert(alert.id);
    }

    fn prune(&mut self, now: i64) {
        self.covered.retain(|_, at| now - *at < CAUSE_MEMORY_SECS);
    }

    fn next_deadline(&self) -> Option<i64> {
        self.pending.iter().map(|p| p.deadline).min()
    }
}

/// Spawn the coalescer: receives alerts from every monitor loop, dispatches
/// the grouped notifications. The grouping window is read live from the
/// config (`alerts.group_window_secs`; 0 disables grouping entirely).
pub fn spawn(
    config: watch::Receiver<Arc<Config>>,
    notifier: Notifiers,
    mut alerts: mpsc::UnboundedReceiver<AlertMsg>,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut state = Coalescer::default();
        loop {
            let now = chrono::Utc::now().timestamp();
            let sleep_for = state.next_deadline().map_or(IDLE_SWEEP, |deadline| {
                Duration::from_secs(u64::try_from(deadline - now).unwrap_or(0).max(1))
            });
            tokio::select! {
                message = alerts.recv() => {
                    let now = chrono::Utc::now().timestamp();
                    {
                        let config = config.borrow();
                        state.retain_monitors(|id| config.monitors.iter().any(|m| m.id == id));
                    }
                    match message {
                        Some(AlertMsg::Down(alert)) => {
                            let window = config.borrow().alerts.group_window_secs;
                            if let Some(alert) = state.on_down(alert, now, window) {
                                send_down(&notifier, &config, &alert).await;
                            }
                        }
                        Some(AlertMsg::Repeat(alert)) => {
                            if state.can_repeat(&alert.id) {
                                send_down(&notifier, &config, &alert).await;
                            }
                        }
                        Some(AlertMsg::Recovered { id, name, notify }) => {
                            let recipients = state.on_recovered(&id, now);
                            send_recovered(&notifier, &config, &name, notify, recipients).await;
                        }
                        Some(AlertMsg::ResumedLocalOnly { id, confirmed }) => {
                            state.resume_local_only(id, confirmed);
                        }
                        None => break,
                    }
                }
                () = tokio::time::sleep(sleep_for) => {}
                _ = shutdown.changed() => break,
            }
            let now = chrono::Utc::now().timestamp();
            for alert in state.flush(now) {
                send_down(&notifier, &config, &alert).await;
            }
        }
        for alert in state.drain(chrono::Utc::now().timestamp()) {
            send_down(&notifier, &config, &alert).await;
        }
    })
}

/// The quiet channels for local-only alerts, read live; `None` when the key
/// is not set (local-only alerts are then only recorded).
fn quiet_routes(config: &watch::Receiver<Arc<Config>>) -> Option<Vec<String>> {
    config.borrow().alerts.notify_unconfirmed.clone()
}

async fn send_down(notifier: &Notifiers, config: &watch::Receiver<Arc<Config>>, alert: &DownAlert) {
    if config
        .borrow()
        .in_maintenance(&alert.id, chrono::Utc::now())
    {
        return;
    }
    let routes = if alert.local_only {
        let Some(quiet) = quiet_routes(config) else {
            info!(monitor = %alert.id, "down seen from this node only: recorded, not sent");
            return;
        };
        info!(monitor = %alert.id, "down seen from this node only: sent to notify_unconfirmed");
        Some(quiet)
    } else {
        alert.notify.clone()
    };
    let impacted: Vec<&str> = alert.impacted.iter().map(String::as_str).collect();
    notifier
        .load_full()
        .dispatch(
            Event::Down {
                monitor: &alert.name,
                error: alert.error.as_deref(),
                cause: alert.cause_name.as_deref(),
                impacted: &impacted,
                vantage: alert.vantage.as_deref(),
                event: alert.event.as_deref(),
                local_only: alert.local_only,
            },
            routes.as_deref(),
        )
        .await;
}

/// Announce a recovery to whoever received the down (see [`Recipients`]).
async fn send_recovered(
    notifier: &Notifiers,
    config: &watch::Receiver<Arc<Config>>,
    name: &str,
    notify: Option<Vec<String>>,
    recipients: Recipients,
) {
    let routes = match recipients {
        Recipients::Nobody => return,
        Recipients::Usual => notify,
        Recipients::Quiet => match quiet_routes(config) {
            Some(quiet) => Some(quiet),
            None => return,
        },
        // `None` is every channel, which includes the quiet ones.
        Recipients::Both => notify.map(|mut routes| {
            for route in quiet_routes(config).unwrap_or_default() {
                if !routes.contains(&route) {
                    routes.push(route);
                }
            }
            routes
        }),
    };
    notifier
        .load_full()
        .dispatch(
            Event::Recovered {
                monitor: name,
                local_only: recipients == Recipients::Quiet,
            },
            routes.as_deref(),
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn down(id: &str, upstreams: &[&str]) -> DownAlert {
        DownAlert {
            id: id.to_owned(),
            name: id.to_uppercase(),
            error: None,
            upstreams: upstreams.iter().map(|&u| u.to_owned()).collect(),
            cause_name: upstreams.first().map(|u| u.to_uppercase()),
            impacted: Vec::new(),
            notify: None,
            vantage: None,
            event: None,
            local_only: false,
        }
    }

    fn local(id: &str, upstreams: &[&str]) -> DownAlert {
        DownAlert {
            local_only: true,
            ..down(id, upstreams)
        }
    }

    #[test]
    fn a_local_only_down_skips_grouping_and_covers_nothing() {
        let mut c = Coalescer::default();
        // Out at once (to the quiet channels), even with an upstream.
        assert!(c.on_down(local("db", &[]), 100, 30).is_some());
        assert!(c.on_down(local("api", &["db"]), 100, 30).is_some());
        // A dependent's real down is not folded into a local-only one.
        assert!(c.on_down(down("web", &["db"]), 110, 30).is_none());
        assert_eq!(c.flush(141).len(), 1);
        // Recoveries go back to whoever received the down.
        assert_eq!(c.on_recovered("db", 200), Recipients::Quiet);
        assert_eq!(c.on_recovered("web", 200), Recipients::Usual);
    }

    #[test]
    fn a_local_only_down_confirmed_later_recovers_everywhere() {
        let mut c = Coalescer::default();
        assert!(c.on_down(local("db", &[]), 100, 30).is_some());
        // Re-asked, the peers now see it down too: the real alert goes out.
        assert!(c.on_down(down("db", &[]), 400, 30).is_some());
        assert_eq!(c.on_recovered("db", 500), Recipients::Both);
        // The next incident starts clean.
        assert!(c.on_down(down("db", &[]), 600, 30).is_some());
        assert_eq!(c.on_recovered("db", 700), Recipients::Usual);
    }

    #[test]
    fn roots_send_immediately_and_cover_their_dependents() {
        let mut c = Coalescer::default();
        // Root: out immediately.
        assert!(c.on_down(down("db", &[]), 100, 30).is_some());
        // Dependent of a covered upstream: folded on arrival.
        assert!(c.on_down(down("api", &["db"]), 110, 30).is_none());
        assert!(c.flush(1000).is_empty());
        // The folded dependent's recovery is silent; the root's is not.
        assert_eq!(Recipients::Nobody, c.on_recovered("api", 120));
        assert_eq!(Recipients::Usual, c.on_recovered("db", 130));
    }

    #[test]
    fn dependent_waits_then_sends_when_no_upstream_alerts() {
        let mut c = Coalescer::default();
        assert!(c.on_down(down("api", &["db"]), 100, 30).is_none());
        assert!(c.flush(120).is_empty()); // window still open
        let sent = c.flush(131);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].id, "api");
        // Its recovery is announced normally.
        assert_eq!(Recipients::Usual, c.on_recovered("api", 200));
    }

    #[test]
    fn dependent_folds_when_upstream_alerts_within_window() {
        let mut c = Coalescer::default();
        // The dependent confirms FIRST - the realistic cascade race, where db
        // does not even have enough recorded failures yet to be derived down.
        assert!(c.on_down(down("api", &["db"]), 100, 30).is_none());
        assert!(c.on_down(down("db", &[]), 110, 30).is_some());
        assert!(c.flush(131).is_empty(), "api folded into db");
        assert_eq!(Recipients::Nobody, c.on_recovered("api", 200));
    }

    #[test]
    fn cascades_collapse_into_one_alert() {
        let mut c = Coalescer::default();
        // db -> api -> web chain: only db's alert goes out.
        assert!(c.on_down(down("db", &[]), 100, 30).is_some());
        assert!(c.on_down(down("api", &["db"]), 105, 30).is_none());
        assert!(c.on_down(down("web", &["api", "db"]), 140, 30).is_none());
        assert!(c.flush(200).is_empty());
    }

    #[test]
    fn flap_within_window_sends_nothing() {
        let mut c = Coalescer::default();
        assert!(c.on_down(down("api", &["db"]), 100, 30).is_none());
        // Recovered before the window closed: drop the down AND the recovery.
        assert_eq!(Recipients::Nobody, c.on_recovered("api", 110));
        assert!(c.flush(131).is_empty());
    }

    #[test]
    fn dependent_waits_for_a_queued_upstream() {
        let mut c = Coalescer::default();
        // web (upstream api) expires before api (upstream db) does.
        assert!(c.on_down(down("web", &["api", "db"]), 100, 10).is_none());
        assert!(c.on_down(down("api", &["db"]), 105, 10).is_none());
        // At 111 web expires but api is still queued: web waits once more.
        assert!(c.flush(111).is_empty());
        // At 116 api expires (db never alerted): api sends, then web folds.
        let sent = c.flush(116);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].id, "api");
        assert!(c.flush(200).is_empty());
        assert_eq!(Recipients::Nobody, c.on_recovered("web", 300));
    }

    #[test]
    fn a_sent_down_clears_a_stale_suppression() {
        let mut c = Coalescer::default();
        // api's down folds into db's: api is suppressed...
        assert!(c.on_down(down("db", &[]), 100, 30).is_some());
        assert!(c.on_down(down("api", &["db"]), 105, 30).is_none());
        // ...and its recovery never arrives (the old task was replaced).
        // Much later api goes down on its own and the alert is sent: its
        // recovery must be announced, not swallowed by the stale entry.
        let late = 105 + CAUSE_MEMORY_SECS + 1;
        assert!(c.on_down(down("api", &["db"]), late, 30).is_none());
        assert_eq!(c.flush(late + 31).len(), 1);
        assert_eq!(Recipients::Usual, c.on_recovered("api", late + 100));
    }

    #[test]
    fn removed_monitors_are_forgotten() {
        let mut c = Coalescer::default();
        assert!(c.on_down(down("db", &[]), 100, 30).is_some());
        assert!(c.on_down(down("api", &["db"]), 105, 30).is_none());
        // api leaves the config: its suppression goes with it.
        c.retain_monitors(|id| id == "db");
        assert!(c.suppressed.is_empty());
    }

    #[test]
    fn window_zero_disables_grouping() {
        let mut c = Coalescer::default();
        assert!(c.on_down(down("api", &["db"]), 100, 0).is_some());
    }

    #[test]
    fn cause_memory_expires() {
        let mut c = Coalescer::default();
        assert!(c.on_down(down("db", &[]), 100, 30).is_some());
        // Much later (memory expired), a dependent alerts on its own.
        let late = 100 + CAUSE_MEMORY_SECS + 1;
        assert!(c.on_down(down("api", &["db"]), late, 30).is_none());
        let sent = c.flush(late + 31);
        assert_eq!(sent.len(), 1);
    }

    /// The whole route, through real channels: a local-only down reaches the
    /// quiet channel only (flagged in the webhook), its confirmation by the
    /// peers reaches the usual one, and each recovery goes to whoever got a
    /// down.
    #[tokio::test]
    async fn local_only_downs_page_the_quiet_channel_only() {
        let (main_url, main) = crate::testing::webhook_sink().await;
        let (quiet_url, quiet) = crate::testing::webhook_sink().await;
        let toml = |unconfirmed: &str| {
            format!(
                r#"
                [page]
                [server]
                [alerts]
                group_window_secs = 0
                {unconfirmed}
                [[channels]]
                name = "main"
                type = "webhook"
                url = "{main_url}"
                [[channels]]
                name = "quiet"
                type = "webhook"
                url = "{quiet_url}"
                [[monitors]]
                id = "api"
                name = "API"
                target = "https://example.com"
                interval_secs = 60
                notify = ["main"]
                "#
            )
        };
        let config =
            Arc::new(crate::config::parse(&toml(r#"notify_unconfirmed = ["quiet"]"#)).unwrap());
        let client = crate::http::client(None).unwrap();
        let notifier = crate::notifications::shared(&config, &client);
        let (config_tx, config_rx) = watch::channel(Arc::clone(&config));
        let (tx, rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = spawn(config_rx, notifier, rx, shutdown_rx);
        let api = |local_only| {
            AlertMsg::Down(DownAlert {
                id: "api".to_owned(),
                name: "API".to_owned(),
                notify: Some(vec!["main".to_owned()]),
                local_only,
                ..down("api", &[])
            })
        };
        let recovered = || AlertMsg::Recovered {
            id: "api".to_owned(),
            name: "API".to_owned(),
            notify: Some(vec!["main".to_owned()]),
        };
        // Wait until the coalescer has dispatched everything sent so far.
        let settle = || async {
            for _ in 0..100 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };

        // Seen up by the peers: the quiet channel only, flagged.
        tx.send(api(true)).unwrap();
        settle().await;
        assert_eq!(crate::testing::events(&main), Vec::<String>::new());
        assert_eq!(crate::testing::events(&quiet), ["down"]);
        assert_eq!(quiet.lock().unwrap()[0]["local_only"], true);
        // Its recovery: the quiet channel only, flagged too.
        tx.send(recovered()).unwrap();
        settle().await;
        assert_eq!(crate::testing::events(&main), Vec::<String>::new());
        assert_eq!(crate::testing::events(&quiet), ["down", "recovered"]);
        assert_eq!(quiet.lock().unwrap()[1]["local_only"], true);

        // Local-only, then confirmed when the peers are asked again: one
        // real down on the usual channel; the recovery reaches both.
        tx.send(api(true)).unwrap();
        tx.send(api(false)).unwrap();
        tx.send(recovered()).unwrap();
        settle().await;
        assert_eq!(crate::testing::events(&main), ["down", "recovered"]);
        assert_eq!(main.lock().unwrap()[0]["local_only"], false);
        assert_eq!(
            crate::testing::events(&quiet),
            ["down", "recovered", "down", "recovered"]
        );

        // Without notify_unconfirmed a local-only down is only recorded.
        config_tx
            .send(Arc::new(crate::config::parse(&toml("")).unwrap()))
            .unwrap();
        tx.send(api(true)).unwrap();
        tx.send(recovered()).unwrap();
        settle().await;
        assert_eq!(crate::testing::events(&main).len(), 2);
        assert_eq!(crate::testing::events(&quiet).len(), 4);

        shutdown_tx.send(true).unwrap();
        task.await.unwrap();
    }

    #[test]
    fn alert_policy_reminders_do_not_bypass_grouping() {
        let mut state = Coalescer::default();
        assert!(state.on_down(down("db", &[]), 0, 30).is_some());
        assert!(state.on_down(down("web", &["db"]), 0, 30).is_none());
        assert!(state.can_repeat("db"));
        assert!(!state.can_repeat("web"));
        assert!(state.on_down(down("worker", &["queue"]), 0, 30).is_none());
        assert!(!state.can_repeat("worker"));
    }
}
