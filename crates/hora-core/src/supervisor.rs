//! Supervises monitor tasks and hot-reloads the configuration.
//!
//! The live config is shared through a [`watch`] channel every component reads.
//! On SIGHUP or a change to the config file, the file is re-read and the running
//! monitor tasks are reconciled: new monitors start, removed ones stop, changed
//! ones restart - unchanged monitors keep running, so a reload never interrupts
//! existing checks. Each monitor's (and peer's) alert state lives here, outside
//! its task, so a restarted task neither re-announces an outage nor loses its
//! recovery; a task that died (a panic) is noticed, logged and restarted.
//!
//! `server.bind` is read once at startup; changing it still requires a restart.
//! Everything else - monitors, peers (the surveillance mesh), notification
//! channels, intervals, thresholds, retention, the certificate window - reloads
//! live: peer-watch tasks are reconciled like monitors, and the outbound
//! heartbeat reads `[health]` on every cycle.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::db::Store;
use reqwest::Client;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::coalesce::{self, AlertMsg};
use crate::config::{self, Config, ConfigIssue, Monitor, Peer};
use crate::config_watch::ConfigWatch;
use crate::mesh::peer::spawn_watch;
use crate::notifications::{self, Notifiers};
use crate::scheduler::{self, AlertCell};

/// A running task and the spec (monitor or peer) it was spawned for.
struct Running<T> {
    spec: T,
    task: JoinHandle<()>,
}

/// The tasks of one kind (monitors or peer watches), with the alert state of
/// each id. The state outlives the tasks: it is dropped only when the id leaves
/// the configuration.
struct Fleet<T> {
    running: HashMap<String, Running<T>>,
    alert_states: HashMap<String, AlertCell>,
}

impl<T> Default for Fleet<T> {
    fn default() -> Self {
        Self {
            running: HashMap::new(),
            alert_states: HashMap::new(),
        }
    }
}

impl<T: PartialEq> Fleet<T> {
    /// Stop the tasks whose id left `desired` or whose spec changed, and drop
    /// the alert state of the ids that left.
    fn retire(&mut self, desired: &HashMap<&str, &T>) {
        self.running
            .retain(|id, run| match desired.get(id.as_str()) {
                Some(spec) if **spec == run.spec => true,
                _ => {
                    run.task.abort();
                    false
                }
            });
        self.alert_states
            .retain(|id, _| desired.contains_key(id.as_str()));
    }

    /// The alert state for `id`, created empty on first use.
    fn alert_state(&mut self, id: &str) -> AlertCell {
        self.alert_states.entry(id.to_owned()).or_default().clone()
    }

    /// Remove (and log) every task that finished on its own - only a panic
    /// does that before shutdown - so the next reconcile restarts it with its
    /// alert state. Returns whether any was found.
    async fn reap_dead(&mut self, what: &str, health: &crate::self_check::SelfHealth) -> bool {
        let dead: Vec<String> = self
            .running
            .iter()
            .filter(|(_, run)| run.task.is_finished())
            .map(|(id, _)| id.clone())
            .collect();
        for id in &dead {
            if let Some(run) = self.running.remove(id) {
                // A JoinError's Display carries the panic message.
                let cause = run
                    .task
                    .await
                    .err()
                    .map_or_else(|| "it returned".to_owned(), |err| err.to_string());
                error!(id = %id, "{what} task stopped unexpectedly ({cause}); restarting it");
                health.task_died(what, id, chrono::Utc::now().timestamp());
            }
        }
        !dead.is_empty()
    }

    /// Await every task (they observe the shutdown signal themselves).
    async fn drain(&mut self) {
        for (_, run) in self.running.drain() {
            let _ = run.task.await;
        }
    }
}

/// How often the supervisor checks for tasks that died (a panic in a probe or
/// in the timer, which nothing else would notice: a monitor would silently
/// stop being checked).
const LIVENESS_CHECK: Duration = Duration::from_secs(30);

/// The shared handles threaded from [`start`] through the reload loop into each
/// spawned monitor: the database pool, the notifier client (used to rebuild
/// channels on reload), the hot-swappable notifier set, and the scheduler
/// liveness beacon.
struct Deps {
    store: Store,
    mqtt: Arc<crate::mqtt::MqttHub>,
    client: Client,
    /// The monitors' probe clients, one per proxy (see [`ProbeClients`]).
    probe_clients: Mutex<ProbeClients>,
    notifier: Notifiers,
    /// Inbox of the alert coalescer (root-cause grouping); every monitor loop
    /// gets a clone.
    alerts: mpsc::UnboundedSender<AlertMsg>,
    last_tick: Arc<AtomicU64>,
    self_health: Arc<crate::self_check::SelfHealth>,
}

/// A handle to the running supervisor: the live config and the hot-swappable
/// notifier set, both shared with the rest of the application.
pub struct Handle {
    pub config: watch::Receiver<Arc<Config>>,
    pub notifier: Notifiers,
    /// The supervise task; await it on shutdown to drain the monitor tasks.
    pub task: JoinHandle<()>,
    /// Task deaths and a dead coalescer, for the self-check and the heartbeat.
    pub self_health: Arc<crate::self_check::SelfHealth>,
    /// The applied config text, for the self-check's missed-change detection.
    pub config_watch: Arc<ConfigWatch>,
}

/// Start supervising: build the notifier set, spawn the initial monitors and the
/// reload loop, and return the handles every component reads.
#[must_use]
pub fn start(
    mut initial: Config,
    config_path: PathBuf,
    store: Store,
    client: Client,
    last_tick: Arc<AtomicU64>,
    shutdown: watch::Receiver<bool>,
) -> Handle {
    // Why edits might not arrive (single-file mount, network filesystem, no
    // watcher) is a standing issue of every load, so the config monitor shows it.
    let (reloads, config_watch, live_issues) = crate::config_watch::start(&config_path);
    for issue in &live_issues {
        error!("config: {issue}");
    }
    initial.issues.extend(live_issues.iter().cloned());
    let notifier = notifications::shared(&initial, &client);
    let (tx, rx) = watch::channel(Arc::new(initial));
    let (alerts_tx, alerts_rx) = mpsc::unbounded_channel();
    let coalescer = coalesce::spawn(
        rx.clone(),
        Arc::clone(&notifier),
        alerts_rx,
        shutdown.clone(),
    );
    let (mqtt, mqtt_task) = crate::mqtt::spawn(rx.clone(), shutdown.clone());
    let deps = Deps {
        store,
        mqtt,
        client,
        probe_clients: Mutex::default(),
        notifier: Arc::clone(&notifier),
        alerts: alerts_tx,
        last_tick,
        self_health: Arc::default(),
    };
    let self_health = Arc::clone(&deps.self_health);
    let supervisor_rx = rx.clone();
    let reload = Reload {
        config_path,
        reloads,
        config_watch: Arc::clone(&config_watch),
        live_issues,
    };
    let task = tokio::spawn(async move {
        supervise(tx, supervisor_rx, reload, deps, coalescer, shutdown).await;
        let _ = mqtt_task.await;
    });
    Handle {
        config: rx,
        notifier,
        task,
        self_health,
        config_watch,
    }
}

/// Where reloads come from and what every load carries along.
struct Reload {
    config_path: PathBuf,
    reloads: mpsc::Receiver<()>,
    config_watch: Arc<ConfigWatch>,
    /// Why edits might not arrive; re-added to every load's issues.
    live_issues: Vec<ConfigIssue>,
}

async fn supervise(
    tx: watch::Sender<Arc<Config>>,
    rx: watch::Receiver<Arc<Config>>,
    reload: Reload,
    deps: Deps,
    coalescer: JoinHandle<()>,
    mut shutdown: watch::Receiver<bool>,
) {
    let Reload {
        config_path,
        mut reloads,
        config_watch,
        live_issues,
    } = reload;
    let mut monitors: Fleet<Monitor> = Fleet::default();
    let mut peers: Fleet<Peer> = Fleet::default();
    reconcile(&mut monitors, &rx, &deps, &shutdown);
    reconcile_peers(&mut peers, &rx, &deps, &shutdown);

    // The raw text last applied: a file event whose content is unchanged (a touch,
    // or a spurious event from some filesystems) is ignored, so a flapping watcher
    // can never spin the supervisor.
    let mut last_raw = std::fs::read_to_string(&config_path).unwrap_or_default();
    config_watch.applied(&last_raw);

    let mut liveness = tokio::time::interval(LIVENESS_CHECK);
    liveness.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut coalescer_dead_logged = false;
    loop {
        let signal = tokio::select! {
            signal = reloads.recv() => signal,
            _ = liveness.tick() => {
                // A task finishing because shutdown began is not a death.
                if *shutdown.borrow() {
                    break;
                }
                if monitors.reap_dead("monitor", &deps.self_health).await {
                    reconcile(&mut monitors, &rx, &deps, &shutdown);
                }
                if peers.reap_dead("peer watch", &deps.self_health).await {
                    reconcile_peers(&mut peers, &rx, &deps, &shutdown);
                }
                if coalescer.is_finished() && !coalescer_dead_logged {
                    coalescer_dead_logged = true;
                    deps.self_health.coalescer_died();
                    error!("the alert coalescer stopped: down and recovered alerts can no longer be delivered until restart");
                }
                continue;
            }
            _ = shutdown.changed() => break,
        };
        if signal.is_none() {
            break;
        }
        // Debounce: wait until the events stop for a moment, so one edit (which
        // fires several events) becomes a single reload, and a file still being
        // written in place is not read half-way.
        settle(&mut reloads).await;

        let raw = match std::fs::read_to_string(&config_path) {
            Ok(raw) => raw,
            Err(err) => {
                // Loudly, through the config monitor: monitors keep running, but
                // edits cannot be applied until the file is readable again.
                // Re-read once it is readable, even if the text is the same as before.
                last_raw.clear();
                let current = with_file_issue(&rx.borrow(), unreadable_reason(&err));
                match current.map(|current| tx.send(Arc::new(current))) {
                    Some(Err(_)) => break,
                    _ => continue, // Published, or reported already.
                }
            }
        };
        if raw == last_raw {
            continue; // Content unchanged: nothing to do.
        }

        match config::parse_lenient(&raw) {
            Ok(mut config) => {
                config_watch.applied(&raw);
                last_raw = raw;
                config.issues.extend(live_issues.iter().cloned());
                log_issues(&config);
                let config = Arc::new(config);
                // Rebuild the channels too, so credential/channel changes
                // apply live. The per-channel failure counters are carried over
                // so a channel that has been failing survives the reload.
                let health = deps.notifier.load().health();
                deps.notifier.store(Arc::new(notifications::build(
                    &config,
                    &deps.client,
                    Some(health),
                )));
                if tx.send(Arc::clone(&config)).is_err() {
                    break;
                }
                reconcile(&mut monitors, &rx, &deps, &shutdown);
                reconcile_peers(&mut peers, &rx, &deps, &shutdown);
                info!(
                    "configuration reloaded ({} monitors, {} peers, {} channels)",
                    config.monitors.len(),
                    config.peers.iter().filter(|peer| peer.is_watched()).count(),
                    deps.notifier.load().len(),
                );
            }
            Err(err) => {
                // Nothing of the file can be loaded (syntax, global settings):
                // the monitors keep running as loaded before, and the
                // configuration monitor says so - loudly, not silently.
                error!("config reload failed, monitors keep their previous configuration: {err:#}");
                config_watch.applied(&raw);
                last_raw = raw;
                let reason = format!(
                    "cannot be loaded ({err:#}); monitors still run the configuration loaded before"
                );
                if let Some(current) = with_file_issue(&rx.borrow(), reason)
                    && tx.send(Arc::new(current)).is_err()
                {
                    break;
                }
            }
        }
    }

    // On shutdown the monitor and peer-watch tasks observe the same signal and
    // break; await them, then the coalescer (which drains its queue).
    monitors.drain().await;
    peers.drain().await;
    let _ = coalescer.await;
}

/// One ERROR line per entry a lenient load dropped or changed.
pub fn log_issues(config: &Config) {
    for issue in &config.issues {
        error!("config: {issue}");
    }
    if config.cannot_alert {
        error!(
            "config: no notification channel or no monitor could be loaded - Hora cannot \
             alert by itself and withholds its external heartbeat"
        );
    }
}

/// Reload once the file events have been quiet this long, so a burst of events
/// collapses into a single reload and a write in progress is not read half-way.
const RELOAD_QUIET: Duration = Duration::from_millis(500);
/// ...but never wait longer than this for a file that keeps changing.
const RELOAD_MAX_WAIT: Duration = Duration::from_secs(5);

async fn settle(reloads: &mut mpsc::Receiver<()>) {
    let deadline = tokio::time::Instant::now() + RELOAD_MAX_WAIT;
    while let Ok(Some(())) = tokio::time::timeout_at(
        deadline.min(tokio::time::Instant::now() + RELOAD_QUIET),
        reloads.recv(),
    )
    .await
    {}
}

/// The running config with `reason` as its config-file issue, or `None` when
/// exactly that is reported already (an unreadable file is retried every minute).
fn with_file_issue(current: &Config, reason: String) -> Option<Config> {
    if current
        .issues
        .iter()
        .any(|issue| issue.item == "config file" && issue.reason == reason)
    {
        return None;
    }
    error!("config file {reason}");
    let mut current = current.clone();
    current.issues.retain(|issue| issue.item != "config file");
    current.issues.push(ConfigIssue {
        item: "config file".to_owned(),
        reason,
    });
    Some(current)
}

/// The config monitor's text for a file that cannot be read.
fn unreadable_reason(err: &std::io::Error) -> String {
    // ESTALE: a file bind-mounted on its own was replaced on the host.
    let hint = if err.raw_os_error() == Some(116) {
        " - the file was replaced on the host while mounted on its own; restart the container, \
         and mount the directory instead of the file"
    } else {
        ""
    };
    format!("cannot be read ({err}){hint}; monitors still run the configuration loaded before")
}

/// Diff running tasks against the latest config: stop removed or changed
/// monitors, start new or changed ones, leave unchanged ones untouched. A
/// restarted monitor resumes its alert state (see [`AlertCell`]).
fn reconcile(
    fleet: &mut Fleet<Monitor>,
    rx: &watch::Receiver<Arc<Config>>,
    deps: &Deps,
    shutdown: &watch::Receiver<bool>,
) {
    let config = rx.borrow().clone();

    let desired: HashMap<&str, &Monitor> =
        config.monitors.iter().map(|m| (m.id.as_str(), m)).collect();
    fleet.retire(&desired);

    let mut probe_clients = deps
        .probe_clients
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    probe_clients.retain_used(&config);
    for monitor in &config.monitors {
        if !fleet.running.contains_key(&monitor.id) {
            // The proxy URL is validated at config load, so this rarely fails.
            let client = match probe_clients.get(monitor.proxy(), monitor.ignore_tls()) {
                Ok(client) => client,
                Err(err) => {
                    warn!(monitor = %monitor.id, "monitor not started, bad proxy: {err:#}");
                    continue;
                }
            };
            let task = scheduler::spawn_monitor(
                monitor.clone(),
                rx.clone(),
                scheduler::MonitorDeps {
                    store: deps.store.clone(),
                    mqtt: Arc::clone(&deps.mqtt),
                    client,
                    confirm_client: deps.client.clone(),
                    notifier: Arc::clone(&deps.notifier),
                    alerts: deps.alerts.clone(),
                    last_tick: Arc::clone(&deps.last_tick),
                    alert_state: fleet.alert_state(&monitor.id),
                },
                shutdown.clone(),
            );
            fleet.running.insert(
                monitor.id.clone(),
                Running {
                    spec: monitor.clone(),
                    task,
                },
            );
        }
    }
}

/// The probe clients monitors share: one for every monitor without a proxy,
/// one per proxy URL for the others. A client carries its own TLS setup and
/// connection pool - about 45 KB - so one per monitor cost 450 MB at 10,000
/// monitors. Monitors on one client share its pool, as requests to the same
/// host from one process do anyway; nothing else about a probe client is
/// per monitor (credentials and timeouts are set per request).
#[derive(Default)]
struct ProbeClients {
    clients: HashMap<(Option<String>, bool), Client>,
}

impl ProbeClients {
    /// The client for `proxy`, built on first use.
    fn get(&mut self, proxy: Option<&str>, ignore_tls: bool) -> reqwest::Result<Client> {
        let key = (proxy.map(str::to_owned), ignore_tls);
        if let Some(client) = self.clients.get(&key) {
            return Ok(client.clone());
        }
        let client = crate::http::probe_client_tls(proxy, ignore_tls)?;
        self.clients.insert(key, client.clone());
        Ok(client)
    }

    /// Forget the clients of proxies no monitor uses any more (running
    /// monitors keep their own handle on theirs until they restart).
    fn retain_used(&mut self, config: &Config) {
        self.clients.retain(|(proxy, ignore_tls), _| {
            proxy.is_none()
                || config.monitors.iter().any(|monitor| {
                    monitor.proxy() == proxy.as_deref() && monitor.ignore_tls() == *ignore_tls
                })
        });
    }
}

/// Diff running peer-watch tasks against the latest config: stop removed or
/// changed peers, start new or changed ones, leave unchanged ones running. Only
/// watched peers (those with `expect_every_secs`) get a task.
fn reconcile_peers(
    fleet: &mut Fleet<Peer>,
    rx: &watch::Receiver<Arc<Config>>,
    deps: &Deps,
    shutdown: &watch::Receiver<bool>,
) {
    let config = rx.borrow().clone();

    let desired: HashMap<&str, &Peer> = config
        .peers
        .iter()
        .filter(|peer| peer.is_watched())
        .map(|peer| (peer.id.as_str(), peer))
        .collect();
    fleet.retire(&desired);

    for peer in config.peers.iter().filter(|peer| peer.is_watched()) {
        if !fleet.running.contains_key(&peer.id) {
            let task = spawn_watch(
                peer.clone(),
                rx.clone(),
                deps.store.clone(),
                deps.client.clone(),
                Arc::clone(&deps.notifier),
                fleet.alert_state(&peer.id),
                shutdown.clone(),
            );
            fleet.running.insert(
                peer.id.clone(),
                Running {
                    spec: peer.clone(),
                    task,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitors_share_a_probe_client_per_proxy() {
        let mut clients = ProbeClients::default();
        clients.get(None, false).unwrap();
        clients.get(None, false).unwrap();
        clients
            .get(Some("http://proxy.example:3128"), false)
            .unwrap();
        clients
            .get(Some("http://proxy.example:3128"), false)
            .unwrap();
        assert_eq!(
            clients.clients.len(),
            2,
            "one client per proxy, not per monitor"
        );

        // A reload that drops the proxied monitor forgets its client.
        let config = config::parse(
            "[page]\n[server]\n[[monitors]]\nid = \"a\"\nname = \"A\"\n\
             target = \"https://example.com\"\ninterval_secs = 60\n",
        )
        .unwrap();
        clients.retain_used(&config);
        assert_eq!(clients.clients.len(), 1);
        assert!(clients.clients.contains_key(&(None, false)));
    }
}
