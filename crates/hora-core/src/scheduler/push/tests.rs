use super::*;
use crate::coalesce::AlertMsg;
use crate::config::{Config, MonitorKind};
use crate::db;
use crate::scheduler::{AlertCell, AlertLevel, MonitorDeps, spawn_monitor};
use std::sync::Arc;
use tokio::sync::{mpsc, watch};

fn config(options: &str) -> Config {
    crate::config::parse(&format!(
        "[page]\n[server]\n[alerts]\nfail_threshold=1\n\
         [[monitors]]\nid='job'\nname='Synthetic job'\nkind='push'\ninterval_secs=3600\n{options}"
    ))
    .unwrap()
}

struct Running {
    task: tokio::task::JoinHandle<()>,
    shutdown: watch::Sender<bool>,
    config: watch::Sender<Arc<Config>>,
    alerts: mpsc::UnboundedReceiver<AlertMsg>,
    state: AlertCell,
}

impl Running {
    fn start(store: &Store, config: Config) -> Self {
        let (config, receiver) = watch::channel(Arc::new(config));
        let (shutdown, stopping) = watch::channel(false);
        let (alerts, inbox) = mpsc::unbounded_channel();
        let client = crate::http::client(None).unwrap();
        let state = AlertCell::default();
        let task = spawn_monitor(
            receiver.borrow().monitors[0].clone(),
            receiver.clone(),
            MonitorDeps {
                store: store.clone(),
                mqtt: Arc::default(),
                client: client.clone(),
                confirm_client: client.clone(),
                notifier: crate::notifications::shared(&receiver.borrow(), &client),
                alerts,
                last_tick: Arc::new(AtomicU64::new(0)),
                alert_state: state.clone(),
            },
            stopping,
        );
        Self {
            task,
            shutdown,
            config,
            alerts: inbox,
            state,
        }
    }

    async fn alert(&mut self) -> AlertMsg {
        tokio::time::timeout(Duration::from_secs(5), self.alerts.recv())
            .await
            .unwrap()
            .unwrap()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn wait_until(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("state transition");
}

#[tokio::test]
async fn healthy_and_unknown_watches_do_not_poll_sqlite() {
    for seen in [false, true] {
        let store = Store::in_memory().await;
        if seen {
            db::insert_push(&store, "job", CheckStatus::Up, None, None)
                .await
                .unwrap();
        }
        let config = config("push_poll_secs=1");
        let monitor = &config.monitors[0];
        let MonitorKind::Push(spec) = &monitor.spec else {
            panic!("push");
        };
        let mut driver = PushDriver::new(&store, monitor, spec).await;
        if let Some(outcome) = driver.evaluate(&store, monitor).await {
            driver.plan(monitor, &outcome, false, false);
        }
        let original_deadline = driver.deadline;
        let reads = store.heartbeat_reads.load(Ordering::Relaxed);
        let tick = AtomicU64::new(0);
        assert!(
            tokio::time::timeout(Duration::from_millis(2100), driver.wait(&tick))
                .await
                .is_err()
        );
        assert_eq!(store.heartbeat_reads.load(Ordering::Relaxed), reads);
        // A push arriving before the wait is polled is not lost. Its persisted
        // timestamp moves the deadline; no heartbeat payload is cached in RAM.
        db::insert_push(&store, "job", CheckStatus::Up, None, None)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), driver.wait(&tick))
            .await
            .unwrap();
        let outcome = driver.evaluate(&store, monitor).await.unwrap();
        assert!(outcome.is_up());
        assert!(driver.deadline > original_deadline);
        assert_eq!(store.heartbeat_reads.load(Ordering::Relaxed), reads + 1);
    }
}

#[tokio::test]
async fn expiry_wakes_at_the_deadline_even_with_a_long_followup_period() {
    let store = Store::in_memory().await;
    let cfg = config("push_poll_secs=60");
    db::insert_push(&store, "job", CheckStatus::Up, None, None)
        .await
        .unwrap();
    // Resume close to expiry: the follow-up period must not delay the deadline.
    sqlx::query("UPDATE checks SET time = time - 3599 WHERE monitor_id = 'job'")
        .execute(store.sqlx())
        .await
        .unwrap();
    let original = db::last_heartbeat(&store, "job").await.unwrap().unwrap();
    let mut running = Running::start(&store, cfg);
    assert!(matches!(running.alert().await, AlertMsg::Down(_)));
    assert!(chrono::Utc::now().timestamp() > original.time + 3600);
    assert_eq!(
        db::last_heartbeat(&store, "job").await.unwrap().unwrap(),
        original
    );
}

#[tokio::test]
async fn push_errors_and_recovery_wake_without_waiting_for_the_poll_period() {
    let store = Store::in_memory().await;
    let mut running = Running::start(&store, config("push_poll_secs=60"));
    wait_until(|| store.heartbeat_reads.load(Ordering::Relaxed) > 0).await;
    db::insert_push(
        &store,
        "job",
        CheckStatus::Down,
        None,
        Some("synthetic failure"),
    )
    .await
    .unwrap();
    assert!(matches!(running.alert().await, AlertMsg::Down(_)));
    db::insert_push(&store, "job", CheckStatus::Up, None, None)
        .await
        .unwrap();
    assert!(matches!(running.alert().await, AlertMsg::Recovered { .. }));
    assert!(
        db::find_open_incident(&store, "job")
            .await
            .unwrap()
            .is_none()
    );
    running.shutdown.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(1), &mut running.task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn retry_cadence_survives_push_bursts_and_reminders_stop_on_recovery() {
    let store = Store::in_memory().await;
    let mut running = Running::start(
        &store,
        config("push_poll_secs=1\nfail_threshold=3\nretry_interval_secs=1\nrepeat_alert_secs=2"),
    );
    let started = Instant::now();
    db::insert_push(&store, "job", CheckStatus::Down, None, None)
        .await
        .unwrap();
    wait_until(|| {
        running
            .state
            .get()
            .is_some_and(|state| state.consecutive_down == 1)
    })
    .await;
    for _ in 0..8 {
        db::insert_push(&store, "job", CheckStatus::Down, None, None)
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(running.state.get().unwrap().consecutive_down, 1);
    assert!(running.alerts.try_recv().is_err());
    assert!(matches!(running.alert().await, AlertMsg::Down(_)));
    assert!(started.elapsed() >= Duration::from_secs(2));
    assert!(matches!(running.alert().await, AlertMsg::Repeat(_)));
    db::insert_push(&store, "job", CheckStatus::Up, None, None)
        .await
        .unwrap();
    assert!(matches!(running.alert().await, AlertMsg::Recovered { .. }));
    assert!(
        tokio::time::timeout(Duration::from_millis(2200), running.alerts.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_recovery_during_silence_is_announced_after_the_silence_is_cleared() {
    let store = Store::in_memory().await;
    let mut running = Running::start(&store, config("push_poll_secs=1"));
    db::insert_push(&store, "job", CheckStatus::Down, None, None)
        .await
        .unwrap();
    assert!(matches!(running.alert().await, AlertMsg::Down(_)));
    db::insert_silence(&store, "job", chrono::Utc::now().timestamp() + 3600, None)
        .await
        .unwrap();
    db::insert_push(&store, "job", CheckStatus::Up, None, None)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        db::find_open_incident(&store, "job")
            .await
            .unwrap()
            .is_none()
    );
    assert!(running.alerts.try_recv().is_err());
    db::clear_silences(&store, chrono::Utc::now().timestamp())
        .await
        .unwrap();
    assert!(matches!(running.alert().await, AlertMsg::Recovered { .. }));
}

#[tokio::test]
async fn maintenance_expiry_resumes_failure_evaluation_without_another_push() {
    let store = Store::in_memory().await;
    let now = chrono::Utc::now();
    let config = config(&format!(
        "push_poll_secs=1\n[[maintenance]]\ntitle='Synthetic maintenance'\nstart='{}'\nend='{}'\n",
        (now - chrono::TimeDelta::seconds(60)).to_rfc3339(),
        (now + chrono::TimeDelta::seconds(2)).to_rfc3339(),
    ));
    let mut running = Running::start(&store, config);
    db::insert_push(&store, "job", CheckStatus::Down, None, None)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(500), running.alerts.recv())
            .await
            .is_err()
    );
    assert!(matches!(running.alert().await, AlertMsg::Down(_)));
}

#[tokio::test]
async fn restart_keeps_the_first_deadline_and_a_global_config_edit_wakes_recovery() {
    let store = Store::in_memory().await;
    let first_expected = chrono::Utc::now().timestamp() - 7200;
    db::meta_set(
        &store,
        "heartbeat_expected_since:job",
        &first_expected.to_string(),
    )
    .await
    .unwrap();
    let mut running = Running::start(&store, config("push_poll_secs=60"));
    assert!(matches!(running.alert().await, AlertMsg::Down(_)));
    let active = config(
        "push_poll_secs=60\n[[maintenance]]\ntitle='Synthetic'\nstart='2000-01-01T00:00:00Z'\nend='2100-01-01T00:00:00Z'",
    );
    running.config.send(Arc::new(active)).unwrap();
    db::insert_push(&store, "job", CheckStatus::Up, None, None)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(running.alerts.try_recv().is_err());
    running
        .config
        .send(Arc::new(config("push_poll_secs=60")))
        .unwrap();
    assert!(matches!(running.alert().await, AlertMsg::Recovered { .. }));
    assert_eq!(
        db::meta_get(&store, "heartbeat_expected_since:job")
            .await
            .unwrap(),
        Some(first_expected.to_string())
    );
}

#[tokio::test]
async fn degraded_pushes_keep_their_threshold_and_recover() {
    let store = Store::in_memory().await;
    let mut cfg = config("push_poll_secs=1\nfail_threshold=2");
    cfg.alerts.alert_on_degraded = true;
    let mut running = Running::start(&store, cfg);
    db::insert_push(
        &store,
        "job",
        CheckStatus::Degraded,
        Some(500),
        Some("synthetic slow job"),
    )
    .await
    .unwrap();
    wait_until(|| {
        running
            .state
            .get()
            .is_some_and(|state| state.consecutive_degraded == 1)
    })
    .await;
    for _ in 0..10 {
        db::insert_push(&store, "job", CheckStatus::Degraded, None, None)
            .await
            .unwrap();
    }
    assert_eq!(running.state.get().unwrap().consecutive_degraded, 1);
    wait_until(|| {
        running
            .state
            .get()
            .is_some_and(|state| state.level == AlertLevel::Degraded)
    })
    .await;
    db::insert_push(&store, "job", CheckStatus::Up, None, None)
        .await
        .unwrap();
    assert!(matches!(running.alert().await, AlertMsg::Recovered { .. }));
}

#[test]
fn utc_deadlines_follow_clock_changes_but_retry_intervals_are_monotonic() {
    let instant = Instant::now();
    let utc = |seconds| chrono::DateTime::from_timestamp(seconds, 0).unwrap();
    let deadline = NextCheck::Utc(Some(3600));
    assert_eq!(
        deadline.remaining(utc(0), instant),
        Duration::from_secs(3600)
    );
    assert_eq!(deadline.remaining(utc(3601), instant), Duration::ZERO);
    assert_eq!(
        deadline.remaining(utc(-60), instant),
        Duration::from_mins(61)
    );
    let retry = NextCheck::Retry(instant + Duration::from_secs(30));
    assert_eq!(retry.remaining(utc(3601), instant), Duration::from_secs(30));
    assert_eq!(retry.remaining(utc(-60), instant), Duration::from_secs(30));
}
