//! `SQLite` persistence layer (sqlx). All timestamps are unix epoch seconds (UTC).
//!
//! Connection and backup live here; the queries are grouped by table family in
//! the submodules and re-exported, so callers keep using `db::insert_check`,
//! `db::recent_incidents`, ...
//!
//! # The storage seam
//!
//! Every caller - the scheduler, the web layer, the CLI - holds a [`Store`] and
//! goes through the functions of this module; none of them sees sqlx or a
//! `SqlitePool`. Errors surface as [`Error`] / [`Result`]. That boundary is
//! where a second backend (a columnar "scale" mode for very large histories)
//! would plug in. `SQLite` stays the default and the reference: a second
//! backend would have to provide, with the same semantics (UTC epoch seconds,
//! `status` 0 down / 1 up / 2 degraded, newest-first orderings):
//!
//! - lifecycle: [`connect`] (schema creation and migrations), [`backup_into`],
//!   [`check_writable`], [`Store::ping`], and the maintenance surface of
//!   `hora compact` ([`Store::size`], [`size_report`], [`apply_retention`],
//!   [`vacuum_swap`] - the lock, [`lock_exclusive`], is a file beside the
//!   database and backend-neutral);
//! - the time series: [`insert_check`], [`insert_push`],
//!   [`insert_heartbeat_miss`], [`recent_checks`], [`check_samples`],
//!   [`last_heartbeat`];
//! - the aggregates the status page reads: [`window_stats_all`] and the
//!   [`WindowCache`] / [`DailyCache`] / [`SparklineCache`] fills,
//!   [`availability_all`], [`daily_all`], [`latency_series`],
//!   [`latency_hourly`], [`latency_percentiles_all`],
//!   [`latency_sparkline_all`];
//! - the roll-ups and retention: [`roll_up_recent`], [`downsample_hourly`],
//!   [`downsample_daily`], the prune of raw rows and orphans;
//! - the small relational tables: incidents, events, announcements,
//!   silences, pushed alerts, certificates, domain expiry, release watch and
//!   the `meta` key/value store.
//!
//! The query functions take `&Store`, so a second implementation means turning
//! `Store` into an enum (or a trait object) over the backends, not touching
//! the callers. The conformance tests in `db/tests.rs` run against a
//! [`Store`], so they are the suite a second backend would have to pass.

#[macro_use]
mod sql;
mod aggregates;
mod annotations;
mod certs;
mod checks;
mod compact;
mod incidents;
mod push_updates;
mod release;
mod retention;
#[cfg(test)]
mod tests;
mod window;

pub use aggregates::{
    DayRow, Point, availability, availability_all, daily_all, latency_hourly,
    latency_percentiles_all, latency_series, latency_sparkline_all,
};
pub use annotations::{
    Announcement, EventMarker, PushedAlert, Silence, active_announcements, active_silences,
    announcements_since, clear_announcements, clear_silences, events_since, insert_announcement,
    insert_event, insert_pushed_alert, insert_silence, insert_silences, is_silenced,
    latest_event_before, meta_get, meta_set, recent_events, recent_pushed_alerts, silences_since,
};
pub use certs::{
    cert_all, cert_pin_fingerprint, domain_expiry, upsert_cert, upsert_cert_pin,
    upsert_domain_expiry,
};
pub use checks::{
    CheckSample, Heartbeat, Latest, check_samples, derive_status, insert_check,
    insert_heartbeat_miss, insert_push, last_check_time, last_check_times, last_heartbeat,
    last_heartbeat_time, recent_checks,
};
pub use compact::{
    Compacted, DbLock, LockError, ObjectSize, SizeReport, StoreSize, apply_retention,
    free_disk_bytes, lock_exclusive, lock_writers, size_report, vacuum_swap,
};
pub use incidents::{
    Incident, IncidentMarks, IncidentMarksCache, find_open_incident, incident_by_id,
    incident_local_only, incident_marks, incidents_between, insert_incident_start,
    latest_incident_id, monitor_incidents, recent_incidents, set_incident_local_only,
    set_incident_note, update_incident_end, update_incident_vantage,
};
pub(crate) use push_updates::PushSubscription;
pub use release::{StoredRelease, mark_release_notified, release_watch, upsert_release_watch};
pub(crate) use retention::prune;
pub use retention::{
    AGGREGATE_RETENTION_DAYS, downsample_daily, downsample_hourly, fill_latency_histograms,
    prune_daily, roll_up_recent,
};
pub use window::{DailyCache, SparklineCache, WindowCache, WindowStats, window_stats_all};

use std::time::Duration;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

/// `meta` key prefix recording since when a push monitor or watched peer has
/// been expecting its first heartbeat (`heartbeat_expected_since:<id>`).
pub(crate) const HEARTBEAT_EXPECTED_META_PREFIX: &str = "heartbeat_expected_since:";
/// `meta` key prefix recording the `cert_pin` the stored `cert_pins` key was
/// last checked against (`cert_pin_against:<id>` = the configured pin), so a
/// reported mismatch is not taken as reported against a *different* pin.
pub(crate) const CERT_PIN_AGAINST_META_PREFIX: &str = "cert_pin_against:";

/// A storage error. Today the sqlx error itself; a second backend would make
/// this an enum over the backends' errors.
pub type Error = sqlx::Error;
/// A storage result.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// The handle on the database every caller holds: cheap to clone (a shared
/// connection pool underneath), safe to use from any task. See the module
/// documentation for the seam it draws.
#[derive(Clone, Debug)]
pub struct Store {
    pool: SqlitePool,
    push_updates: std::sync::Arc<push_updates::PushUpdates>,
    #[cfg(test)]
    pub(crate) heartbeat_reads: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Store {
    fn from_pool(pool: SqlitePool) -> Self {
        Self {
            pool,
            push_updates: std::sync::Arc::default(),
            #[cfg(test)]
            heartbeat_reads: std::sync::Arc::default(),
        }
    }

    pub(crate) fn subscribe_push(&self, id: &str) -> PushSubscription {
        self.push_updates.subscribe(id)
    }

    /// The sqlx pool, for the query functions of this module only.
    pub(crate) fn sqlx(&self) -> &SqlitePool {
        &self.pool
    }

    /// A trivial round trip, to tell a wedged or locked database from a live one.
    ///
    /// # Errors
    ///
    /// Whatever the database returns.
    pub async fn ping(&self) -> Result<()> {
        sqlx::query_scalar::<_, i64>("SELECT 1")
            .fetch_one(&self.pool)
            .await
            .map(drop)
    }

    /// Close every connection, waiting for in-flight queries to finish.
    pub async fn close(&self) {
        self.pool.close().await;
    }

    /// A fresh, migrated in-memory database on a single connection (an
    /// in-memory `SQLite` database lives and dies with its connection, so a
    /// pool of several would see several databases). For tests.
    ///
    /// # Panics
    ///
    /// When the in-memory database cannot be opened or migrated.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn in_memory() -> Self {
        let options = SqliteConnectOptions::new()
            .filename(":memory:")
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("open in-memory database");
        migrator()
            .run(&pool)
            .await
            .expect("migrate in-memory database");
        Self::from_pool(pool)
    }

    /// The raw pool, for test fixtures that insert rows no public function
    /// writes (an old-format row, a forged timestamp). Never for product code.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn fixture_pool(&self) -> &SqlitePool {
        &self.pool
    }
}

/// The embedded database migrations.
fn migrator() -> sqlx::migrate::Migrator {
    sqlx::migrate!("./migrations")
}

/// Open the pool (creating the file if needed, WAL mode) and run migrations.
///
/// # Errors
///
/// Returns an error if the database cannot be opened or migrations fail.
pub async fn connect(database_path: &str) -> anyhow::Result<Store> {
    connect_with_settings(database_path, &PoolSettings::default()).await
}

/// Per-connection SQLite page cache and the maximum number of connections.
/// Defaults retain the upstream throughput-oriented budget.
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PoolSettings {
    pub cache_kib: u32,
    pub max_connections: u32,
}

impl Default for PoolSettings {
    fn default() -> Self {
        Self {
            cache_kib: 16_000,
            max_connections: 8,
        }
    }
}

impl PoolSettings {
    /// # Errors
    /// Rejects cache or connection limits outside the supported budget.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            (256..=65_536).contains(&self.cache_kib),
            "database cache_kib must be 256..65536"
        );
        anyhow::ensure!(
            (1..=16).contains(&self.max_connections),
            "database max_connections must be 1..16"
        );
        Ok(())
    }
}

/// Open the database with an explicit, bounded page-cache budget.
///
/// # Errors
/// Returns an error for invalid limits, unavailable storage or failed migrations.
pub async fn connect_with_settings(
    database_path: &str,
    settings: &PoolSettings,
) -> anyhow::Result<Store> {
    settings.validate()?;
    // The database holds failure snippets, push payloads and incident detail, so
    // create it private (0600) rather than at the process umask. SQLite then
    // mirrors that mode onto the -wal/-shm sidecars it spawns.
    let path = database_path.to_owned();
    tokio::task::spawn_blocking(move || precreate_private(&path)).await??;
    let options = SqliteConnectOptions::new()
        .filename(database_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        // NORMAL is the recommended durability level under WAL: writes fsync only
        // at checkpoint, not on every probe insert. Safe for this time series -
        // at worst a power loss drops the last few checks.
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5))
        // Negative values are KiB. SQLite budgets this per connection.
        .pragma("cache_size", format!("-{}", settings.cache_kib))
        // A checkpoint rewinds the WAL but leaves the file at its high-water
        // mark, so one burst (long reads pinning a snapshot while the prune
        // writes) kept 100 MB+ on disk. Truncate it back to 64 MiB after a
        // checkpoint instead.
        .pragma("journal_size_limit", "67108864");

    // WAL lets readers run while one writer (the scheduler) inserts, so a roomy
    // pool keeps the parallel summary queries from queueing.
    let pool = SqlitePoolOptions::new()
        .max_connections(settings.max_connections)
        // Fail fast under contention instead of the 30s default; a slow monitor
        // query then degrades just that card, not the whole page.
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(options)
        .await?;

    migrator().run(&pool).await?;
    // Give the planner statistics before the first page builds. Without
    // `sqlite_stat1` it favors a full scan of the monitor-led index for the
    // 24h aggregates (seconds on a seasoned database); with stats it skip-scans
    // (`ANY(monitor_id) AND time>?`), milliseconds. `PRAGMA optimize` only
    // re-runs ANALYZE when the shape of the data has drifted, so on most boots
    // this is a no-op; the first one pays a sub-second full ANALYZE.
    sqlx::query("PRAGMA optimize").execute(&pool).await?;
    Ok(Store::from_pool(pool))
}

/// Create the database file with owner-only (0600) permissions before sqlx opens
/// it, so secrets in the time series aren't world-readable. A no-op for an
/// in-memory database, an existing file, or a non-Unix platform (where file
/// modes don't apply).
#[cfg(unix)]
fn precreate_private(database_path: &str) -> anyhow::Result<()> {
    use anyhow::Context as _;

    // `file:` URIs are skipped wholesale: parsing them (query params, mode=memory)
    // isn't worth it for a spelling Hora never documents. An operator who points a
    // `file:` URI at a real on-disk database owns its permissions.
    if database_path == ":memory:" || database_path.starts_with("file:") {
        return Ok(());
    }
    match create_private(database_path) {
        Ok(()) => Ok(()),
        // Already there (or another opener won the race): keep its mode.
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err).with_context(|| format!("creating database file {database_path}")),
    }
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn precreate_private(_database_path: &str) -> anyhow::Result<()> {
    Ok(())
}

/// Create `path` as a new, empty file - owner-only (0600) on Unix. Fails with
/// `AlreadyExists` when anything is already there, atomically (no
/// check-then-create window).
fn create_private(path: &str) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path).map(drop)
}

/// Whether the existing database at `database_path` opens and takes the
/// write lock, without writing anything: `BEGIN IMMEDIATE` takes the reserved
/// lock (it fails on a read-only mount), `ROLLBACK` releases it. Never creates
/// nor migrates a database (`hora doctor`).
///
/// # Errors
///
/// The open or lock error, as text.
pub async fn check_writable(database_path: &str) -> std::result::Result<(), String> {
    let options = SqliteConnectOptions::new()
        .filename(database_path)
        .busy_timeout(Duration::from_secs(2));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .map_err(|err| err.to_string())?;
    let writable = sqlx::raw_sql("BEGIN IMMEDIATE; ROLLBACK;")
        .execute(&pool)
        .await;
    pool.close().await;
    writable
        .map(drop)
        .map_err(|err| format!("not writable: {err}"))
}

/// Copy the database into `dest` with `VACUUM INTO`: a consistent, compacted
/// snapshot taken through `SQLite` itself, safe while the daemon is writing
/// (a WAL reader does not block the writer). The source is opened read-only,
/// so a backup never creates or migrates a database.
///
/// # Errors
///
/// Returns an error if `dest` already exists, the source cannot be opened, or
/// the copy fails.
pub async fn backup_into(database_path: &str, dest: &str) -> anyhow::Result<()> {
    use anyhow::Context as _;

    // Claim the destination first, empty and owner-only (0600) like the live
    // database: VACUUM INTO accepts an empty file and keeps its mode, so the
    // snapshot is never readable at the process umask, not even briefly.
    // `create_new` also makes the "never overwrite a backup" check atomic.
    let target = dest.to_owned();
    match tokio::task::spawn_blocking(move || create_private(&target)).await? {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            anyhow::bail!("destination {dest} already exists; refusing to overwrite a backup")
        }
        Err(err) => return Err(err).with_context(|| format!("creating {dest}")),
    }

    let copied = vacuum_into(database_path, dest).await;
    if copied.is_err() {
        // Don't leave the empty placeholder behind: it would block a retry.
        let target = dest.to_owned();
        let _ = tokio::task::spawn_blocking(move || std::fs::remove_file(target)).await;
    }
    copied
}

async fn vacuum_into(database_path: &str, dest: &str) -> anyhow::Result<()> {
    use anyhow::Context as _;

    let options = SqliteConnectOptions::new()
        .filename(database_path)
        .read_only(true)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .with_context(|| format!("opening {database_path} read-only"))?;
    let copied = sqlx::query("VACUUM INTO ?")
        .bind(dest)
        .execute(&pool)
        .await
        .with_context(|| format!("copying into {dest}"));
    pool.close().await;
    copied.map(|_| ())
}
