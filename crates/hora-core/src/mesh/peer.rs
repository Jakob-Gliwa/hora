//! Mutual surveillance: the outbound dead-man heartbeat, the peer watches, and
//! the quorum check that distinguishes a real peer outage from a network
//! partition.
//!
//! The wire is deliberately plain HTTP: a heartbeat is a `POST` to a URL (a
//! peer's `/api/push/{id}`, or any external receiver), and a witness query is a
//! `GET /healthz` whose JSON body carries this node's view of its peers. Nothing
//! here is a bespoke protocol, so any half-channel can terminate at an external
//! service (healthchecks.io, `UptimeRobot`) instead of another Hora.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::db::Store;
use futures_util::future::join_all;
use hora_notify::Event;
use reqwest::Client;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use super::wire::{HealthReport, HealthStatus, PeerSeen, PeerState};
use crate::config::{Config, Health, Peer};
use crate::db;
use crate::heartbeat::{Cadence, heartbeat_expected_since, heartbeat_outcome_for};
use crate::notifications::Notifiers;
use crate::scheduler::{AlertCell, AlertLevel, AlertState, silenced};

/// Floor on the scheduler-liveness tolerance, so a node whose fastest monitor
/// ticks very frequently is not flagged unhealthy by a momentary scheduling jitter.
const MIN_LIVENESS_TOLERANCE: i64 = 30;

/// How long a witness `/healthz` poll may take before it is treated as unreachable.
const WITNESS_TIMEOUT: Duration = Duration::from_secs(5);

/// How long an outbound heartbeat `POST` may take before it is abandoned.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);

/// Hard deadline for one whole round of outbound heartbeats (they are sent
/// concurrently, so this is one slow receiver's worth plus slack): a dead
/// receiver can never delay the dead-man cadence to the others.
const HEARTBEAT_ROUND_DEADLINE: Duration = Duration::from_secs(12);

/// A witness `/healthz` report is a small JSON object; cap the body so a
/// compromised peer can't stream hundreds of MB into memory within the timeout.
const MAX_REPORT_BYTES: usize = 64 * 1024;

// --- /healthz report ------------------------------------------------------

/// Build the `/healthz` report for the current configuration and scheduler state.
pub async fn report(store: &Store, config: &Config, last_tick: &AtomicU64) -> HealthReport {
    let now = chrono::Utc::now().timestamp();
    let (scheduler_ok, last_tick_age) = scheduler_liveness(config, last_tick, now);
    let db_ok = db_ok(store).await;

    let mut peers = HashMap::new();
    for peer in config.peers.iter().filter(|peer| peer.is_watched()) {
        peers.insert(peer.id.clone(), peer_seen(store, peer, now).await);
    }

    HealthReport {
        status: if scheduler_ok && db_ok {
            HealthStatus::Ok
        } else {
            HealthStatus::Degraded
        },
        scheduler_ok,
        db_ok,
        last_tick_age,
        id: config.health.as_ref().map(|health| health.id.clone()),
        peers,
    }
}

/// Whether the scheduler loop is demonstrably alive, plus the age of the last
/// tick. The liveness beacon (`last_tick`) advances at the cadence of the
/// *fastest* monitor (it is a `fetch_max` across all of them), so the tolerance is
/// derived from the smallest interval - not the largest, which would let a slow
/// daily monitor mask a wedged scheduler for hours. Idle push tasks update the
/// beacon on their cheap clock check, without polling SQLite. A node with no
/// monitors has nothing to go stale, so it is trivially alive.
pub(crate) fn scheduler_liveness(config: &Config, last_tick: &AtomicU64, now: i64) -> (bool, i64) {
    let last = last_tick.load(Ordering::Relaxed);
    if last == 0 {
        // Never ticked: at startup, "alive" only once there is nothing to warm up.
        return (config.monitors.is_empty(), -1);
    }
    let age = (now - i64::try_from(last).unwrap_or(i64::MAX)).max(0);
    let Some(min_interval) = config
        .monitors
        .iter()
        .map(|monitor| {
            if matches!(monitor.spec, crate::config::MonitorKind::Push(_)) {
                crate::scheduler::PUSH_CLOCK_CHECK_SECS
            } else {
                monitor.interval_secs
            }
        })
        .min()
    else {
        return (true, age);
    };
    let tolerance = i64::try_from(min_interval.saturating_mul(2))
        .unwrap_or(i64::MAX)
        .max(MIN_LIVENESS_TOLERANCE);
    (age <= tolerance, age)
}

/// Whether the database answers a trivial query within a short timeout. A wedged
/// or locked database makes this false, which (with the scheduler check) stops the
/// outbound heartbeat - so the dead-man fires rather than a zombie pinging "ok".
pub(crate) async fn db_ok(store: &Store) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(2), store.ping()).await,
        Ok(Ok(()))
    )
}

/// This node's view of one peer, measured from its last *positive* heartbeat
/// (recorded misses are ignored, see [`db::last_heartbeat_time`]): `up` within the
/// expected interval, `down` once that lapses, `unknown` if never seen. `age` is
/// the seconds since that last real heartbeat, so it stays meaningful through an
/// outage instead of resetting on each recorded miss.
async fn peer_seen(store: &Store, peer: &Peer, now: i64) -> PeerSeen {
    let expect = i64::try_from(peer.expect_every_secs.unwrap_or(0)).unwrap_or(i64::MAX);
    match db::last_heartbeat_time(store, peer.listen_id()).await {
        Ok(Some(last)) => {
            let age = (now - last).max(0);
            PeerSeen {
                state: if now - last > expect {
                    PeerState::Down
                } else {
                    PeerState::Up
                },
                age,
            }
        }
        _ => PeerSeen {
            state: PeerState::Unknown,
            age: -1,
        },
    }
}

// --- Outbound heartbeat (the dead-man) ------------------------------------

/// Spawn the outbound heartbeat: every `health.interval_secs`, when this node is
/// locally healthy, `POST` a heartbeat to each peer's `ping_url` and to the
/// optional `heartbeat_url`. When unhealthy it stays silent - that silence is the
/// down signal the receiver detects. A shutdown signal stops it between ticks.
#[must_use]
pub fn spawn_heartbeat(
    config: watch::Receiver<Arc<Config>>,
    store: Store,
    client: Client,
    last_tick: Arc<AtomicU64>,
    self_health: Arc<crate::self_check::SelfHealth>,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            // Re-read the interval each cycle so a changed `interval_secs` (or a
            // newly added/removed `[health]`) applies live without a restart.
            let interval = config
                .borrow()
                .health
                .as_ref()
                .map_or_else(|| Duration::from_mins(1), Health::interval);
            tokio::select! {
                () = tokio::time::sleep(interval) => {}
                _ = shutdown.changed() => break,
            }
            let snapshot = config.borrow().clone();
            let Some(health) = snapshot.health.as_ref() else {
                continue;
            };

            let now = chrono::Utc::now().timestamp();
            let (scheduler_ok, age) = scheduler_liveness(&snapshot, &last_tick, now);
            let db_ok = db_ok(&store).await;
            if !(scheduler_ok && db_ok) {
                warn!(scheduler_ok, db_ok, "heartbeat skipped: node unhealthy");
                continue;
            }
            // No channel or no monitor survived the config load: Hora cannot
            // alert by itself, so the dead-man service has to.
            if snapshot.cannot_alert {
                warn!("heartbeat withheld: no notification channel or monitor could be loaded");
                continue;
            }
            // Every channel failing, or the coalescer gone: Hora's own alerts
            // reach nobody, so the dead-man service has to.
            if self_health.cannot_deliver() {
                warn!("heartbeat withheld: alerts cannot be delivered");
                continue;
            }

            let digest = format!("ok mon={} tick={}s", snapshot.monitors.len(), age.max(0));
            let targets = snapshot
                .peers
                .iter()
                .filter_map(|peer| {
                    peer.ping_url.as_ref().map(|url| {
                        (
                            url.as_ref(),
                            peer.ping_token.as_ref().map(AsRef::as_ref),
                            Some(digest.as_str()),
                        )
                    })
                })
                .chain(
                    health
                        .heartbeat_url
                        .as_ref()
                        .map(|url| (url.as_ref(), None, None)),
                );
            // Concurrently, under one deadline: sequential 10 s timeouts would
            // let a single dead receiver push every other heartbeat late.
            let round = join_all(
                targets.map(|(url, token, digest)| send_heartbeat(&client, url, token, digest)),
            );
            if tokio::time::timeout(HEARTBEAT_ROUND_DEADLINE, round)
                .await
                .is_err()
            {
                warn!("heartbeat round abandoned past its deadline");
            }
        }
    })
}

/// Fire one heartbeat: an empty POST to external receivers, or
/// `POST {url}?status=up&msg={digest}` to a peer with the token (if any)
/// in the `X-Push-Token` header, so it stays out of the receiver's access logs.
/// Fire-and-forget: a failure is logged (without the URL, which may carry
/// credentials) and the next tick tries again.
async fn send_heartbeat(client: &Client, url: &str, token: Option<&str>, digest: Option<&str>) {
    let mut request = client.post(url).timeout(HEARTBEAT_TIMEOUT);
    if let Some(digest) = digest {
        request = request.query(&[("status", "up"), ("msg", digest)]);
    }
    if let Some(token) = token {
        request = request.header("x-push-token", token);
    }
    match request.send().await {
        Ok(response) if response.status().is_success() => {}
        Ok(response) => warn!(status = %response.status(), "heartbeat ping rejected"),
        // The error Display embeds the URL (possibly with credentials), so omit it.
        Err(_err) => warn!("heartbeat ping failed (network error)"),
    }
}

// --- Peer watches (the inbound side, with quorum) -------------------------

/// The quorum verdict for a peer this node can no longer reach.
enum Verdict {
    /// No witness vouches for it (or none to consult): a real outage.
    Confirmed,
    /// A witness still sees it up: a partition on the local-to-peer link.
    Partition(String),
    /// No witness reachable at all: this node is likely the isolated one.
    Isolated,
}

/// Spawn the watch task for one peer (the IN side). The supervisor owns these and
/// reconciles them on config reload, so adding, removing or changing a peer takes
/// effect without a restart (the captured `peer` is replaced by a fresh task,
/// which picks up the previous one's alert state from `alert_state`).
#[must_use]
pub(crate) fn spawn_watch(
    peer: Peer,
    config: watch::Receiver<Arc<Config>>,
    store: Store,
    client: Client,
    notifier: Notifiers,
    alert_state: AlertCell,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // `is_watched()` guarantees `expect_every_secs` is set.
        let expect = peer.expect_every_secs.unwrap_or(60);
        let cadence = Cadence::Every(expect);
        // When this watch started: heartbeats within `grace_secs` of it are not
        // alerted, so a node whose persisted history looks instantly stale after a
        // restart (e.g. both peers rebooting together) doesn't fire a false down.
        let started = chrono::Utc::now().timestamp();
        // A peer that never pinged at all is judged from when its first
        // heartbeat was expected (persisted across restarts), not left unknown.
        let expected_since = heartbeat_expected_since(&store, peer.listen_id(), started).await;
        let mut ticker = tokio::time::interval(Duration::from_secs(expect));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut state = alert_state.get().unwrap_or_default();

        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = shutdown.changed() => break,
            }
            let now = chrono::Utc::now();
            // `None` = never pinged and not yet late (or a read error): nothing
            // to react to.
            let Some(outcome) = heartbeat_outcome_for(
                &store,
                peer.listen_id(),
                &cadence,
                expected_since,
                now.timestamp(),
            )
            .await
            else {
                continue;
            };

            let snapshot = config.borrow().clone();
            // Muted by a maintenance window or an ad-hoc silence, exactly like a
            // monitor: the miss is still recorded, only alerts are skipped.
            if snapshot.in_maintenance(peer.listen_id(), now)
                || silenced(&store, peer.listen_id()).await
            {
                continue;
            }
            let threshold = snapshot.alerts.fail_threshold.max(1);

            if outcome.is_up() {
                let previous = state.level;
                if state.observe_up() {
                    // Only announce recovery if we actually alerted (Isolated was silent).
                    if matches!(previous, AlertLevel::Down | AlertLevel::Partition) {
                        info!(peer = %peer.id, "peer recovered");
                        dispatch(
                            &notifier,
                            &peer,
                            Event::Recovered {
                                monitor: &peer.name,
                                local_only: false,
                            },
                        )
                        .await;
                    }
                    state.level = AlertLevel::Healthy;
                }
                alert_state.set(state);
                continue;
            }

            let confirmed = state.observe_down(threshold);
            alert_state.set(state);
            // Hold off during the post-startup grace window (see `started`).
            let grace = i64::try_from(
                snapshot
                    .health
                    .as_ref()
                    .map_or(0, |health| health.grace_secs),
            )
            .unwrap_or(0);
            let in_grace = now.timestamp() - started < grace;
            if !confirmed || in_grace {
                continue;
            }

            // Confirmed down by the local view; consult witnesses before alerting if
            // quorum is on. Re-evaluated every tick until it resolves (so a partition
            // that becomes a real outage escalates).
            let verdict = if snapshot.health.as_ref().is_some_and(|health| health.quorum) {
                confirm_down(&client, &snapshot, &peer).await
            } else {
                Verdict::Confirmed
            };
            announce_verdict(&notifier, &peer, &outcome, verdict, &mut state).await;
            alert_state.set(state);
        }
    })
}

/// Act on the quorum verdict for a peer confirmed down locally, moving `state`
/// to the level announced. A suppressed state (partition, isolation) is
/// announced once, not on every tick it persists.
async fn announce_verdict(
    notifier: &Notifiers,
    peer: &Peer,
    outcome: &crate::probe::Outcome,
    verdict: Verdict,
    state: &mut AlertState,
) {
    match verdict {
        Verdict::Confirmed => {
            error!(peer = %peer.id, "peer down");
            dispatch(
                notifier,
                peer,
                Event::Down {
                    monitor: &peer.name,
                    error: outcome.error.as_deref(),
                    cause: None,
                    impacted: &[],
                    vantage: None,
                    event: None,
                    local_only: false,
                },
            )
            .await;
            state.level = AlertLevel::Down;
        }
        Verdict::Partition(witness) if state.level != AlertLevel::Partition => {
            warn!(peer = %peer.id, %witness, "peer unreachable but a witness sees it up: treating as a partition, not an outage");
            dispatch(
                notifier,
                peer,
                Event::PeerLinkDegraded {
                    peer: &peer.name,
                    witness: &witness,
                },
            )
            .await;
            state.level = AlertLevel::Partition;
        }
        Verdict::Isolated if state.level != AlertLevel::Isolated => {
            warn!(peer = %peer.id, "peer down but no witness reachable: possible local isolation, not alerting");
            state.level = AlertLevel::Isolated;
        }
        // Already in this suppressed state: nothing to re-announce.
        Verdict::Partition(_) | Verdict::Isolated => {}
    }
}

/// Ask the other peers whether they still see `target`. Witnesses are the peers
/// (other than the target) with a resolvable `/healthz`: if any reports the target
/// up, this is a partition; if none is reachable, this node is likely isolated;
/// otherwise the outage is confirmed. With no witnesses to consult it is confirmed
/// (the honest two-node behaviour).
async fn confirm_down(client: &Client, config: &Config, target: &Peer) -> Verdict {
    let witnesses: Vec<(String, String)> = config
        .peers
        .iter()
        .filter(|peer| peer.id != target.id)
        .filter_map(|peer| {
            peer.effective_witness_url()
                .map(|url| (peer.name.clone(), url))
        })
        .collect();
    if witnesses.is_empty() {
        return Verdict::Confirmed;
    }

    // Poll every witness concurrently, so one slow node can't delay the verdict.
    let polls = witnesses
        .iter()
        .map(|(name, url)| async move { (name.clone(), fetch_witness(client, url).await) });
    let results = join_all(polls).await;

    let mut reachable = 0u32;
    let mut partition = None;
    for (name, report) in results {
        if let Some(seen) = report {
            reachable += 1;
            if partition.is_none()
                && seen
                    .peers
                    .get(&target.id)
                    .is_some_and(|view| view.state == PeerState::Up)
            {
                partition = Some(name);
            }
        }
    }
    if let Some(name) = partition {
        Verdict::Partition(name)
    } else if reachable == 0 {
        Verdict::Isolated
    } else {
        Verdict::Confirmed
    }
}

/// Fetch a witness's `/healthz` report, or `None` if it is unreachable or replies
/// with something other than a complete, healthy report.
async fn fetch_witness(client: &Client, url: &str) -> Option<HealthReport> {
    crate::http::fetch_json_capped(client.get(url), MAX_REPORT_BYTES, WITNESS_TIMEOUT).await
}

/// Deliver an event to the peer's routed channels (or all, if unrouted).
async fn dispatch(notifier: &Notifiers, peer: &Peer, event: Event<'_>) {
    notifier
        .load_full()
        .dispatch(event, peer.notify.as_deref())
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config;

    async fn insert(store: &Store, id: &str, time: i64, status: i64) {
        sqlx::query(
            "INSERT INTO checks (time, monitor_id, status, latency_ms, status_code, error) \
             VALUES (?, ?, ?, NULL, NULL, NULL)",
        )
        .bind(time)
        .bind(id)
        .bind(status)
        .execute(store.sqlx())
        .await
        .expect("insert check");
    }

    fn cfg(toml: &str) -> Config {
        config::parse(toml).expect("valid config")
    }

    #[tokio::test]
    async fn external_heartbeat_omits_details_while_peer_keeps_protocol() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let config = cfg(&format!(
            r#"
            [page]
            [server]
            [health]
            id = "synthetic-node"
            interval_secs = 1
            heartbeat_url = "http://{address}/external"
            [[peers]]
            id = "peer"
            name = "Synthetic peer"
            ping_url = "http://{address}/peer"
            ping_token = "synthetic-test-token"
            "#
        ));
        let (_updates, receiver) = watch::channel(Arc::new(config));
        let (shutdown, stopping) = watch::channel(false);
        let heartbeat = spawn_heartbeat(
            receiver,
            Store::in_memory().await,
            Client::builder().no_proxy().build().unwrap(),
            Arc::new(AtomicU64::new(0)),
            Arc::default(),
            stopping,
        );
        let requests = tokio::time::timeout(Duration::from_secs(5), async {
            let mut requests = Vec::new();
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    let count = socket.read(&mut buffer).await.unwrap();
                    assert!(count > 0 && request.len() < 8192);
                    request.extend_from_slice(&buffer[..count]);
                }
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await
                    .unwrap();
                requests.push(String::from_utf8(request).unwrap());
            }
            requests.sort();
            requests
        })
        .await;
        shutdown.send(true).unwrap();
        heartbeat.await.unwrap();
        let requests = requests.expect("heartbeat reached both loopback receivers");
        assert!(requests[0].starts_with("POST /external HTTP/1.1\r\n"));
        assert!(!requests[0].contains("synthetic"));
        assert!(!requests[0].contains("msg="));
        assert!(!requests[0].contains("x-push-token"));
        assert!(requests[0].ends_with("\r\n\r\n"));
        assert!(
            requests[1].starts_with("POST /peer?status=up&msg=ok+mon%3D0+tick%3D0s HTTP/1.1\r\n")
        );
        assert!(requests[1].contains("x-push-token: synthetic-test-token\r\n"));
    }

    #[test]
    fn liveness_alive_without_monitors() {
        let config = cfg("[page]\n[server]\n");
        let (ok, age) = scheduler_liveness(&config, &AtomicU64::new(0), 1000);
        assert!(ok);
        assert_eq!(age, -1);
    }

    #[test]
    fn push_liveness_uses_the_idle_beacon_instead_of_the_heartbeat_deadline() {
        let config = cfg(
            "[page]\n[server]\n[[monitors]]\nid='job'\nname='Job'\nkind='push'\ninterval_secs=86400\npush_poll_secs=1",
        );
        let tick = AtomicU64::new(1000);
        assert_eq!(scheduler_liveness(&config, &tick, 1061), (true, 61));
        assert!(!scheduler_liveness(&config, &tick, 1121).0);
        assert!(!scheduler_liveness(&config, &AtomicU64::new(0), 1000).0);
    }

    #[test]
    fn liveness_uses_fastest_interval_and_warms_up() {
        let config = cfg("[page]\n[server]\n\
             [[monitors]]\nid=\"slow\"\nname=\"S\"\ntarget=\"https://e.com\"\ninterval_secs=86400\n\
             [[monitors]]\nid=\"fast\"\nname=\"F\"\ntarget=\"https://e.com\"\ninterval_secs=30\n");
        // Fresh tick: age 10 <= 2*min(30)=60 -> alive (the slow monitor must not relax it).
        let tick = AtomicU64::new(1000);
        assert_eq!(scheduler_liveness(&config, &tick, 1010), (true, 10));
        // Stale: age 200 > 60 -> not alive, even though it is well within the slow interval.
        assert!(!scheduler_liveness(&config, &tick, 1200).0);
        // Never ticked while monitors exist -> warming up, not alive.
        assert_eq!(
            scheduler_liveness(&config, &AtomicU64::new(0), 1000),
            (false, -1)
        );
    }

    #[tokio::test]
    async fn peer_seen_reflects_freshness_and_status() {
        let store = Store::in_memory().await;
        let base = cfg("[page]\n[server]\n[health]\nid=\"a\"\n\
             [[peers]]\nid=\"x\"\nname=\"X\"\nexpect_every_secs=100\n");
        let mut peer = base.peers[0].clone();
        let now = 1_000_000;

        // Never seen.
        peer.id = "never".to_owned();
        assert_eq!(
            peer_seen(&store, &peer, now).await.state,
            PeerState::Unknown
        );

        // Recent up ping.
        peer.id = "up".to_owned();
        insert(&store, "up", now - 10, 1).await;
        let seen = peer_seen(&store, &peer, now).await;
        assert_eq!(seen.state, PeerState::Up);
        assert_eq!(seen.age, 10);

        // A lone recorded miss (no positive heartbeat ever) is still unknown:
        // staleness is measured from real heartbeats, not from recorded misses.
        peer.id = "miss".to_owned();
        insert(&store, "miss", now - 1, 0).await;
        assert_eq!(
            peer_seen(&store, &peer, now).await.state,
            PeerState::Unknown
        );

        // Stale up ping, with a fresh miss on top: age is measured from the last
        // real heartbeat (200s), so it is down despite the recent miss row.
        peer.id = "stale".to_owned();
        insert(&store, "stale", now - 200, 1).await;
        insert(&store, "stale", now - 1, 0).await;
        assert_eq!(peer_seen(&store, &peer, now).await.state, PeerState::Down);
        assert_eq!(peer_seen(&store, &peer, now).await.age, 200);
    }

    #[tokio::test]
    async fn report_includes_watched_peers_and_status() {
        let store = Store::in_memory().await;
        let config = cfg("[page]\n[server]\n[health]\nid=\"hora-a\"\n\
             [[peers]]\nid=\"hora-b\"\nname=\"B\"\nexpect_every_secs=100\n\
             [[peers]]\nid=\"hc\"\nname=\"HC\"\nping_url=\"https://hc-ping.com/x\"\n");
        let report = report(&store, &config, &AtomicU64::new(0)).await;
        assert_eq!(report.id.as_deref(), Some("hora-a"));
        assert!(report.db_ok);
        // No monitors -> scheduler trivially alive, so status is ok.
        assert_eq!(report.status, HealthStatus::Ok);
        // Only the watched peer (hora-b) appears; the OUT-only peer (hc) does not.
        assert!(report.peers.contains_key("hora-b"));
        assert!(!report.peers.contains_key("hc"));
        assert_eq!(report.peers["hora-b"].state, PeerState::Unknown);
    }
}
