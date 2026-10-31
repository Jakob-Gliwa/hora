//! The status summary, built in the background on demand and served from memory.
//!
//! Building the summary reads the database (milliseconds to seconds,
//! depending on its size); serving it should not. A refresher task builds
//! the operator's view and swaps it in whole; a request loads the latest
//! snapshot. Every audience's view, and every rendered body (page, text,
//! JSON, metrics), is derived from that one build in memory, at most once per
//! snapshot.
//!
//! Builds follow demand instead of running continuously: nobody looking at
//! the page should cost a core. A request finding a snapshot older than
//! [`MIN_REFRESH`] wakes the refresher and is answered from the current one
//! (stale-while-revalidate), unless that is older than [`STALE_ON_DEMAND`]:
//! then it waits (at most [`DEMAND_WAIT`]) for the fresh build, so a viewer
//! returning after a quiet hour never sees an old status. Without requests,
//! the refresher rebuilds every [`IDLE_REFRESH`] to keep its incremental
//! caches warm. The very first request after start waits for the first build.
//!
//! A build never starts sooner than its own duration after the previous one
//! ended: on a database slow enough that a build takes longer than
//! [`MIN_REFRESH`], the refresher idles at least half the time instead of
//! keeping a connection and a core busy forever. A config reload, or an
//! operator write that must show at once (an announcement, an event marker),
//! starts a build immediately.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

use arc_swap::ArcSwapOption;
use axum::body::Bytes;
use hora_core::config::Config;
use hora_core::db::Store;
use hora_core::mesh::vantage::VantageMap;
use hora_core::notifications::{self, Notifiers};
use tokio::sync::{Notify, watch};

use crate::AppState;
use crate::layout::Spliced;
use crate::summary::{BuildState, Built, Summary, build_summary, derive};
use crate::visibility::{Audience, Visibility};

/// A snapshot younger than this is served without waking the refresher.
pub(crate) const MIN_REFRESH: Duration = Duration::from_secs(5);
/// Older than this, a request waits (bounded) for the build it triggered.
pub(crate) const STALE_ON_DEMAND: Duration = Duration::from_secs(30);
/// The longest a request waits for that build before taking the old one.
const DEMAND_WAIT: Duration = Duration::from_secs(5);
/// Rebuild cadence while nobody requests anything (keeps caches warm).
pub(crate) const IDLE_REFRESH: Duration = Duration::from_mins(5);
/// How long an operator write waits for the build that shows it.
const WRITE_VISIBLE_TIMEOUT: Duration = Duration::from_mins(1);

/// A rendered body: which view, in which format.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Body {
    /// The status page (whole, or one group's) as HTML, with its per-request
    /// parts left as holes (see `crate::layout::Spliced`). The flag is a
    /// `?token=` that opened nothing (the private door says so): whether one
    /// came, never which.
    Page(Audience, Option<String>, bool),
    /// The same, as aligned plain text.
    Text(Audience, Option<String>),
    /// `/api/summary`.
    Json(Audience),
    /// `/metrics`.
    Metrics(Audience),
}

/// One build and everything derived from it.
pub(crate) struct Snapshot {
    built: Built,
    /// Derived views and rendered bodies, for the config they were derived
    /// against (a reload invalidates them before the next build lands).
    derived: Mutex<Derived>,
}

#[derive(Default)]
struct Derived {
    config: Option<Arc<Config>>,
    summaries: HashMap<Audience, Arc<Summary>>,
    bodies: HashMap<Body, Bytes>,
    pages: HashMap<Body, Arc<Spliced>>,
}

impl Derived {
    /// The memo, emptied first if it was derived against another config.
    fn for_config(&mut self, config: &Arc<Config>) -> &mut Self {
        if !self
            .config
            .as_ref()
            .is_some_and(|derived| Arc::ptr_eq(derived, config))
        {
            *self = Self {
                config: Some(Arc::clone(config)),
                ..Self::default()
            };
        }
        self
    }
}

impl Snapshot {
    fn new(built: Built) -> Self {
        Self {
            built,
            derived: Mutex::new(Derived::default()),
        }
    }

    fn derived(&self) -> std::sync::MutexGuard<'_, Derived> {
        self.derived.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The summary as `audience` sees it under `config`.
    pub(crate) fn summary(&self, config: &Arc<Config>, audience: &Audience) -> Arc<Summary> {
        if *audience == Audience::Operator {
            return Arc::clone(&self.built.summary);
        }
        if let Some(summary) = self.derived().for_config(config).summaries.get(audience) {
            return Arc::clone(summary);
        }
        // Derived outside the lock: a few hundred `Arc` clones, but no reason
        // to serialize concurrent first requests on it.
        let visibility = Visibility::new(config, audience);
        let summary = Arc::new(derive(&self.built, config, &visibility));
        self.derived()
            .for_config(config)
            .summaries
            .insert(audience.clone(), Arc::clone(&summary));
        summary
    }

    /// A rendered body, rendered once per snapshot. `render` returning `None`
    /// (a group nobody here may see) is not cached.
    pub(crate) fn body<E>(
        &self,
        config: &Arc<Config>,
        key: Body,
        render: impl FnOnce() -> Result<Option<Bytes>, E>,
    ) -> Result<Option<Bytes>, E> {
        if let Some(body) = self.derived().for_config(config).bodies.get(&key) {
            return Ok(Some(body.clone()));
        }
        let Some(body) = render()? else {
            return Ok(None);
        };
        self.derived()
            .for_config(config)
            .bodies
            .insert(key, body.clone());
        Ok(Some(body))
    }
}

impl Snapshot {
    /// A page with holes, rendered and cut once per snapshot (see
    /// [`Snapshot::body`]); each request splices its own parts in.
    pub(crate) fn page<E>(
        &self,
        config: &Arc<Config>,
        key: Body,
        render: impl FnOnce() -> Result<Option<String>, E>,
    ) -> Result<Option<Arc<Spliced>>, E> {
        if let Some(page) = self.derived().for_config(config).pages.get(&key) {
            return Ok(Some(Arc::clone(page)));
        }
        let Some(rendered) = render()? else {
            return Ok(None);
        };
        let page = Arc::new(Spliced::new(rendered));
        self.derived()
            .for_config(config)
            .pages
            .insert(key, Arc::clone(&page));
        Ok(Some(page))
    }
}

/// The latest snapshot and the background task keeping it fresh.
pub(crate) struct Refresher {
    current: ArcSwapOption<Snapshot>,
    /// The generation of the latest published snapshot.
    published: watch::Sender<u64>,
    /// The generation of the latest build started.
    started: AtomicU64,
    wake: Arc<Notify>,
    spawned: AtomicBool,
    /// Reference point for `published_ms` (monotonic).
    epoch: Instant,
    /// When the latest snapshot was published, in ms since `epoch`.
    published_ms: AtomicU64,
    /// A build is running: its result answers waiting requests, so they
    /// need not queue another one right behind it.
    building: AtomicBool,
}

impl Default for Refresher {
    fn default() -> Self {
        Self {
            current: ArcSwapOption::empty(),
            published: watch::Sender::new(0),
            started: AtomicU64::new(0),
            wake: Arc::new(Notify::new()),
            spawned: AtomicBool::new(false),
            // An hour back, so a test can date a snapshot into the past.
            epoch: Instant::now()
                .checked_sub(Duration::from_secs(3600))
                .unwrap_or_else(Instant::now),
            published_ms: AtomicU64::new(0),
            building: AtomicBool::new(false),
        }
    }
}

impl Refresher {
    /// Start the background task (once; later calls are no-ops). The daemon
    /// calls it at startup so the first build overlaps the boot; a request
    /// arriving first starts it too.
    pub(crate) fn start(self: &Arc<Self>, state: &AppState) {
        if self.spawned.swap(true, Ordering::AcqRel) {
            return;
        }
        let refresher = Arc::downgrade(self);
        let wake = Arc::clone(&self.wake);
        let store = state.store.clone();
        let config = state.config.clone();
        let notifier = state.notifier.clone();
        let vantage = Arc::clone(&state.vantage);
        tokio::spawn(run(refresher, wake, store, config, notifier, vantage));
    }

    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Date the latest snapshot `ago` into the past.
    #[cfg(test)]
    fn pretend_published_ago(&self, ago: Duration) {
        let ago = u64::try_from(ago.as_millis()).unwrap_or(u64::MAX);
        self.published_ms
            .store(self.elapsed_ms().saturating_sub(ago), Ordering::Release);
    }

    /// How old the latest published snapshot is.
    fn age(&self) -> Duration {
        Duration::from_millis(
            self.elapsed_ms()
                .saturating_sub(self.published_ms.load(Ordering::Acquire)),
        )
    }

    /// The latest snapshot. A stale one wakes the refresher; only a very
    /// stale one (and the first one after start) waits for the next build.
    pub(crate) async fn snapshot(self: &Arc<Self>, state: &AppState) -> Arc<Snapshot> {
        if let Some(snapshot) = self.current.load_full() {
            let age = self.age();
            if age < MIN_REFRESH {
                return snapshot;
            }
            let mut published = self.published.subscribe();
            let target = *published.borrow_and_update() + 1;
            self.start(state);
            if !self.building.load(Ordering::Acquire) {
                self.wake.notify_one();
            }
            if age < STALE_ON_DEMAND {
                return snapshot;
            }
            let fresh = tokio::time::timeout(
                DEMAND_WAIT,
                published.wait_for(|&generation| generation >= target),
            )
            .await;
            return match fresh {
                Ok(_) => self.current.load_full().unwrap_or(snapshot),
                Err(_) => snapshot,
            };
        }
        self.start(state);
        let mut published = self.published.subscribe();
        // The sender lives in `self`, so the wait cannot fail.
        let _ = published.wait_for(|&generation| generation >= 1).await;
        self.current
            .load_full()
            .expect("a published generation has a snapshot")
    }

    /// Rebuild now and wait (bounded) until a snapshot that started after
    /// this call is published - for an operator write that should be visible
    /// on the very next page view.
    pub(crate) async fn refresh_now(self: &Arc<Self>, state: &AppState) {
        self.start(state);
        let target = self.started.load(Ordering::Acquire) + 1;
        let mut published = self.published.subscribe();
        self.wake.notify_one();
        let _ = tokio::time::timeout(
            WRITE_VISIBLE_TIMEOUT,
            published.wait_for(|&generation| generation >= target),
        )
        .await;
    }
}

/// The refresher loop. Holds the refresher weakly, so it ends with the app
/// state. A closed config channel only means no more reloads.
async fn run(
    refresher: Weak<Refresher>,
    wake: Arc<Notify>,
    store: Store,
    mut config: watch::Receiver<Arc<Config>>,
    notifier: Notifiers,
    vantage: VantageMap,
) {
    let mut state = BuildState::default();
    let mut built_for: Option<Arc<Config>> = None;
    let mut reloads = true;
    loop {
        let Some(this) = refresher.upgrade() else {
            return;
        };
        let current = config.borrow_and_update().clone();
        // The caches are keyed by monitor id and follow the windows on their
        // own; only an id coming back (its history may have been swept while
        // it was gone) must not find what they kept of it.
        if built_for
            .as_ref()
            .is_some_and(|previous| adds_monitors(previous, &current))
        {
            state = BuildState::default();
        }
        built_for = Some(Arc::clone(&current));
        this.building.store(true, Ordering::Release);
        let generation = this.started.fetch_add(1, Ordering::AcqRel) + 1;
        let began = Instant::now();
        // Channel health is read live from the dispatcher; the vantage map is
        // the poller's last snapshot - a lock-free read, never the network.
        let health = notifications::health_snapshot(&notifier);
        let peers = vantage.load_full();
        // Built in a task of its own: a panic loses this build's caches, not
        // the refresher (the page would freeze on its last snapshot).
        let task = tokio::spawn({
            let store = store.clone();
            let current = Arc::clone(&current);
            let mut state = std::mem::take(&mut state);
            async move {
                let built = build_summary(&store, &current, &mut state, &health, &peers).await;
                (state, built)
            }
        });
        let took = match task.await {
            Ok((kept, built)) => {
                state = kept;
                this.current.store(Some(Arc::new(Snapshot::new(built))));
                this.published_ms
                    .store(this.elapsed_ms(), Ordering::Release);
                this.published.send_replace(generation);
                let took = began.elapsed();
                if generation == 1 {
                    tracing::info!("status page ready (first build {took:.1?})");
                } else {
                    tracing::debug!("status summary rebuilt in {took:.1?}");
                }
                took
            }
            Err(err) => {
                tracing::error!("status summary build failed: {err}");
                began.elapsed()
            }
        };
        this.building.store(false, Ordering::Release);
        drop(this);
        let ended = Instant::now();

        // Idle until a request finds the snapshot stale, an operator write or
        // a reload asks for one, or the keep-warm period is over.
        tokio::select! {
            () = tokio::time::sleep(IDLE_REFRESH.max(took)) => {}
            () = wake.notified() => {}
            changed = config.changed(), if reloads => {
                reloads = changed.is_ok();
            }
        }
        // Never sooner than the last build's own duration after it ended.
        tokio::time::sleep(took.saturating_sub(ended.elapsed())).await;
    }
}

/// Whether `next` configures a monitor id `previous` did not.
fn adds_monitors(previous: &Config, next: &Config) -> bool {
    let known: std::collections::HashSet<&str> =
        previous.monitors.iter().map(|m| m.id.as_str()).collect();
    next.monitors
        .iter()
        .any(|monitor| !known.contains(monitor.id.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    const ONE: &str = r#"
        [page]
        [server]
        [[monitors]]
        id = "web"
        name = "Web"
        target = "https://example.com"
        interval_secs = 60
    "#;

    async fn state() -> (AppState, Store, watch::Sender<Arc<Config>>) {
        let store = Store::in_memory().await;
        let config = Arc::new(hora_core::config::parse(ONE).expect("config"));
        let client = hora_core::http::client(None).expect("client");
        let notifier = hora_core::notifications::shared(&config, &client);
        let (tx, rx) = watch::channel(config);
        let tick = u64::try_from(chrono::Utc::now().timestamp()).unwrap_or(0);
        let state = AppState::new(store.clone(), rx, Arc::new(AtomicU64::new(tick)), notifier);
        (state, store, tx)
    }

    fn generation(state: &AppState) -> u64 {
        *state.refresher.published.borrow()
    }

    async fn latency(state: &AppState) -> Option<i64> {
        let config = state.config.borrow().clone();
        let summary = state.snapshot().await.summary(&config, &Audience::Operator);
        summary.monitors[0].last_latency_ms
    }

    /// A check `ago` seconds back (one row per monitor and second).
    async fn insert_up(store: &Store, latency: i64, ago: i64) {
        sqlx::query(
            "INSERT INTO checks (time, monitor_id, status, latency_ms) VALUES (?, 'web', 1, ?)",
        )
        .bind(chrono::Utc::now().timestamp() - ago)
        .bind(latency)
        .execute(store.fixture_pool())
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn nobody_looking_means_no_rebuilds() {
        let (state, _store, _tx) = state().await;
        state.snapshot().await;
        assert_eq!(generation(&state), 1);
        // The old refresher rebuilt every MIN_REFRESH regardless of viewers.
        tokio::time::sleep(MIN_REFRESH + Duration::from_secs(1)).await;
        assert_eq!(generation(&state), 1, "rebuilt without a request");
    }

    #[tokio::test]
    async fn a_stale_view_is_served_at_once_and_refreshed_behind_it() {
        let (state, store, _tx) = state().await;
        insert_up(&store, 42, 10).await;
        assert_eq!(latency(&state).await, Some(42));
        insert_up(&store, 7, 0).await;
        state
            .refresher
            .pretend_published_ago(MIN_REFRESH + Duration::from_secs(1));
        // Served from the current snapshot, without waiting...
        assert_eq!(latency(&state).await, Some(42));
        // ...while the refresher builds the next one in the background.
        let rebuilt = tokio::time::timeout(Duration::from_secs(4), async {
            while generation(&state) < 2 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(rebuilt.is_ok(), "the stale view did not wake the refresher");
        assert_eq!(latency(&state).await, Some(7));
    }

    #[tokio::test]
    async fn a_view_after_a_quiet_spell_waits_for_the_fresh_build() {
        let (state, store, _tx) = state().await;
        insert_up(&store, 42, 10).await;
        assert_eq!(latency(&state).await, Some(42));
        insert_up(&store, 7, 0).await;
        state
            .refresher
            .pretend_published_ago(STALE_ON_DEMAND + Duration::from_secs(1));
        assert_eq!(
            latency(&state).await,
            Some(7),
            "served a snapshot older than STALE_ON_DEMAND"
        );
        assert_eq!(generation(&state), 2);
    }
}
