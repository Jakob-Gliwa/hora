//! Configuration: parsed from a TOML file, with environment overrides for secrets.
//!
//! [`Config`] and its loaders live here; the larger pieces of the schema
//! ([`Monitor`], [`Channel`], [`Peer`]), the `${VAR}` expansion and the
//! validation pass each have their own submodule.

mod channel;
mod env;
mod lenient;
pub use lenient::{
    CONFIG_MONITOR_ID, ConfigIssue, ConfigMonitor, parse_lenient, parse_lenient_with_exec_dir,
};
mod maintenance;
mod monitor;
pub use maintenance::Maintenance;
mod parsed;
mod peer;
mod raw;
mod secret;
#[cfg(test)]
mod tests;
mod validate;

pub use channel::Channel;
pub use monitor::{
    DnsSpec, ExecSpec, HttpSpec, IcmpSpec, JsonAssert, KeywordAssert, Kind, Monitor, MonitorKind,
    MqttSpec, NetworkProbe, NumberAssert, PushSchedule, PushSpec, ReleaseWatch, Starttls, TcpSpec,
    split_host_port,
};
pub use parsed::{Compiled, DnsRecord, ParseField, Parsed};
pub use peer::Peer;
pub use secret::{Secret, redact_url_secrets};

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use serde::Deserialize;

use env::expand_env_table;
use validate::validate;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub page: Page,
    pub server: Server,
    /// Named notification channels. Monitors route to them by name.
    #[serde(default)]
    pub channels: Vec<Channel>,
    #[serde(default)]
    pub brokers: Vec<crate::mqtt::Broker>,
    /// Scheduled maintenance windows that mute alerts.
    #[serde(default)]
    pub maintenance: Vec<Maintenance>,
    /// Incidents / announcements shown as a banner on the status page.
    #[serde(default)]
    pub incidents: Vec<Incident>,
    #[serde(default)]
    pub alerts: Alerts,
    #[serde(default)]
    pub monitors: Vec<Monitor>,
    /// Outbound dead-man heartbeat + peer-watch settings. Absent = this node
    /// neither emits heartbeats nor watches peers.
    #[serde(default)]
    pub health: Option<Health>,
    /// Peers in a mutual-surveillance mesh. Each entry pings the peer (OUT) and/or
    /// watches it for missed pings (IN); requires a [`Health`] section.
    #[serde(default)]
    pub peers: Vec<Peer>,
    /// Periodic recap through the notification channels ("this week: 99.97%,
    /// 2 incidents, budget 18m of 43m left"). Absent = no digest.
    #[serde(default)]
    pub digest: Option<Digest>,
    /// Hora's own new releases, told to the operator (status page banner and
    /// one message per release). On by default.
    #[serde(default)]
    pub updates: Updates,
    /// Where exec-probe executables live, from the `HORA_EXEC_DIR`
    /// environment variable - never from the file. The separation is the
    /// security model: the hot-reloadable config alone must not be able to
    /// run code; enabling exec probes takes deployment-level access too.
    #[serde(skip)]
    pub exec_dir: Option<std::path::PathBuf>,
    /// The built-in configuration monitor (see [`CONFIG_MONITOR_ID`]).
    #[serde(default)]
    pub config_monitor: Option<ConfigMonitor>,
    /// What a lenient load dropped or changed ([`parse_lenient`]); empty
    /// after a strict one.
    #[serde(skip)]
    pub issues: Vec<ConfigIssue>,
    /// A lenient load kept no notification channel or no monitor of the ones
    /// configured: Hora cannot alert anybody itself, so it withholds its
    /// external heartbeat and lets the dead-man service raise the alarm.
    #[serde(skip)]
    pub cannot_alert: bool,
}

impl Config {
    /// The configured monitor with this id.
    #[must_use]
    pub fn find_monitor(&self, id: &str) -> Option<&Monitor> {
        self.monitors.iter().find(|monitor| monitor.id == id)
    }

    #[must_use]
    pub fn fail_threshold(&self, id: &str) -> u32 {
        self.find_monitor(id)
            .map_or(self.alerts.fail_threshold.max(1), |m| {
                m.fail_threshold(self.alerts.fail_threshold)
            })
    }

    /// A monitor's display name, falling back to its id (a monitor removed
    /// from the config after its incidents or alerts were recorded).
    #[must_use]
    pub fn monitor_name<'a>(&'a self, id: &'a str) -> &'a str {
        self.find_monitor(id)
            .map_or(id, |monitor| monitor.name.as_str())
    }

    /// The maintenance window covering `monitor_id` at `now`, if any. A window
    /// with no `monitors` list covers all of them.
    #[must_use]
    pub fn active_maintenance(&self, monitor_id: &str, now: DateTime<Utc>) -> Option<&Maintenance> {
        self.maintenance.iter().find(|window| {
            window.active_span(now).is_some()
                && (window.monitors.is_empty() || window.monitors.iter().any(|id| id == monitor_id))
        })
    }

    /// Whether `monitor_id` is inside an active maintenance window (alerts muted).
    #[must_use]
    pub fn in_maintenance(&self, monitor_id: &str, now: DateTime<Utc>) -> bool {
        self.active_maintenance(monitor_id, now).is_some()
    }
}

/// The periodic digest: a recap of the last seven days per monitor (uptime,
/// incidents, error budget), sent through the notification channels on a cron
/// schedule. Zero false-positive risk by construction - it reports, it never
/// alerts.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Digest {
    /// When to send, five-field cron, UTC. Default: Monday 08:00.
    #[serde(default = "default_digest_schedule")]
    pub schedule: String,
    /// Channels to send through; absent = every configured channel.
    #[serde(default)]
    pub notify: Option<Vec<String>>,
}

/// Hora's own update check: once a day at most, GitHub is asked for Hora's
/// latest release; a newer one than this build shows on the operator's status
/// page and is sent once through the channels, with its changelog's opening.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Updates {
    /// Ask GitHub for Hora's latest release. `false` sends nothing out.
    #[serde(default = "default_true")]
    pub check: bool,
    /// Channels to tell; absent = every configured channel, `[]` = none (the
    /// banner alone).
    #[serde(default)]
    pub notify: Option<Vec<String>>,
}

impl Default for Updates {
    fn default() -> Self {
        Self {
            check: true,
            notify: None,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_digest_schedule() -> String {
    "0 8 * * 1".to_owned()
}

/// Severity of an [`Incident`] or an ad-hoc announcement, controlling its
/// banner colour. Parsed from and shown as its lowercase name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    #[default]
    Info,
    Warning,
    Critical,
    Resolved,
}

impl Severity {
    /// Lowercase name, used as a CSS class and in the API.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Critical => "critical",
            Self::Resolved => "resolved",
        }
    }
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Severity {
    type Err = UnknownSeverity;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "info" => Ok(Self::Info),
            "warning" => Ok(Self::Warning),
            "critical" => Ok(Self::Critical),
            "resolved" => Ok(Self::Resolved),
            _ => Err(UnknownSeverity),
        }
    }
}

/// A severity name outside `info`, `warning`, `critical` and `resolved`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownSeverity;

impl UnknownSeverity {
    /// The message every door (API, CLI, TUI) answers with.
    pub const MESSAGE: &'static str = "severity must be info, warning, critical or resolved";
}

impl std::fmt::Display for UnknownSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(Self::MESSAGE)
    }
}

impl std::error::Error for UnknownSeverity {}

/// A posted incident or announcement shown on the status page.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Incident {
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub severity: Severity,
    /// When it was posted (RFC 3339, quoted in TOML); shown if present.
    #[serde(default)]
    pub at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    #[serde(default = "default_title")]
    pub title: String,
    #[serde(default = "default_history_days")]
    pub history_days: u16,
    /// An image shown beside the title in place of its initial tile: an SVG,
    /// PNG, WebP or JPEG file, relative to the config file. Read with the
    /// config, into [`Page::logo_image`].
    #[serde(default)]
    pub logo: Option<PathBuf>,
    /// The logo for the dark theme, the same way; without it, [`Page::logo`]
    /// shows in both themes.
    #[serde(default)]
    pub logo_dark: Option<PathBuf>,
    /// The bytes of [`Page::logo`], once [`load_from`] has read them.
    #[serde(skip)]
    pub logo_image: Option<Logo>,
    /// The bytes of [`Page::logo_dark`], once [`load_from`] has read them.
    #[serde(skip)]
    pub logo_dark_image: Option<Logo>,
}

/// The operator's logo, read from [`Page::logo`] (served at `/logo`) or
/// [`Page::logo_dark`] (at `/logo-dark`).
#[derive(Debug, Clone)]
pub struct Logo {
    pub bytes: std::sync::Arc<[u8]>,
    pub content_type: &'static str,
    /// A digest of the bytes, the cache buster in the page's `?v=`.
    pub version: String,
}

/// The largest logo Hora accepts: a header mark, not a photograph.
const LOGO_MAX_BYTES: u64 = 256 * 1024;

impl Logo {
    /// Read the logo at `path`, typed by its extension.
    ///
    /// # Errors
    ///
    /// Returns an error if the file is missing, too large, or not an SVG,
    /// PNG, WebP or JPEG.
    pub fn read(path: &Path) -> anyhow::Result<Self> {
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase);
        let content_type = match extension.as_deref() {
            Some("svg") => "image/svg+xml",
            Some("png") => "image/png",
            Some("webp") => "image/webp",
            Some("jpg" | "jpeg") => "image/jpeg",
            _ => anyhow::bail!(
                "logo {}: expected an .svg, .png, .webp or .jpg file",
                path.display()
            ),
        };
        let size = std::fs::metadata(path)
            .with_context(|| format!("reading logo {}", path.display()))?
            .len();
        anyhow::ensure!(
            size <= LOGO_MAX_BYTES,
            "logo {}: {size} bytes, the limit is {LOGO_MAX_BYTES}",
            path.display()
        );
        let bytes =
            std::fs::read(path).with_context(|| format!("reading logo {}", path.display()))?;
        let mut version = crate::cert::sha256_hex(&bytes);
        version.truncate(12);
        Ok(Self {
            bytes: bytes.into(),
            content_type,
            version,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_database_path")]
    pub database_path: String,
    /// SQLite page-cache budget; changing it requires a daemon restart.
    #[serde(default)]
    pub database: crate::db::PoolSettings,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Trust this request header for the client IP when rate limiting (e.g.
    /// `cf-connecting-ip` behind Cloudflare). Only safe when a proxy you control
    /// sets it and direct access to the origin is blocked - otherwise clients can
    /// forge it. Unset = the TCP peer address: forwarded headers are never
    /// trusted on their own (behind a proxy every client then shares its
    /// bucket; the daemon logs a warning when it sees one).
    #[serde(default)]
    pub client_ip_header: Option<String>,
    /// Per-IP API rate limit: one request slot is replenished every N seconds.
    #[serde(default = "default_rate_limit_refill")]
    pub rate_limit_refill_secs: u64,
    /// Per-IP API rate limit: maximum burst of requests.
    #[serde(default = "default_rate_limit_burst")]
    pub rate_limit_burst: u32,
    /// Token required to view private monitors (those with `public = false`).
    /// Sent as `Authorization: Bearer <token>` or `?token=` query parameter.
    #[serde(default)]
    pub auth_token: Option<Secret>,
    /// Per-group viewer tokens for the `/status/{group}` pages (lightweight
    /// multi-tenancy): a token here reveals the *full* view of its group -
    /// private monitors included - and nothing else. The global `auth_token`
    /// always works too.
    #[serde(default)]
    pub group_tokens: std::collections::HashMap<String, Secret>,
}

/// Alerting and retention policy, independent of any notification channel.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Alerts {
    /// Consecutive failed checks before a monitor is alerted as down.
    #[serde(default = "default_threshold")]
    pub fail_threshold: u32,
    /// Periodic DOWN reminders; 0 disables them.
    #[serde(default)]
    pub repeat_alert_secs: u64,
    /// Also alert when a monitor is *degraded* - up, but slower than its
    /// `degraded_over_ms` - not only when it is down. Uses the same
    /// `fail_threshold` (consecutive degraded checks). Off by default.
    #[serde(default)]
    pub alert_on_degraded: bool,
    /// Warn this many days before a TLS certificate expires.
    #[serde(default = "default_cert_expiry_days")]
    pub cert_expiry_days: u16,
    /// Warn this many days before a registered domain expires (RDAP); applies
    /// to monitors that opt in with `domain_expiry = "example.com"`.
    #[serde(default = "default_domain_expiry_days")]
    pub domain_expiry_days: u16,
    /// Default storage retention, overridable per monitor.
    #[serde(default = "default_retention_days")]
    pub default_retention_days: u16,
    /// Root-cause alert grouping window, seconds. A monitor confirmed down
    /// with a down upstream waits this long before alerting; if the upstream
    /// alerts first (or already has), the symptom is folded into that single
    /// notification. 0 disables grouping (every monitor alerts on its own,
    /// the pre-0.5 behaviour). Default 30.
    #[serde(default = "default_group_window")]
    pub group_window_secs: u64,
    /// Anti-flood window, seconds, for pushed alerts that carry a `dedup_key`
    /// (`POST /api/monitors/{id}/alert`): a repeat of the same key within the
    /// window is coalesced (dropped, counted) instead of dispatched again, so
    /// a flapping producer pages once, not a hundred times. 0 disables the
    /// coalescing (every pushed alert dispatches). Default 300.
    #[serde(default = "default_push_alert_window")]
    pub push_alert_window_secs: u64,
    /// Consecutive delivery failures before the notification watchdog alerts
    /// the *other* channels that a channel is broken ("your telegram channel
    /// has been failing"). The dead-man's switch applied to notifications
    /// themselves. Default 3.
    #[serde(default = "default_channel_fail_threshold")]
    pub channel_fail_threshold: u32,
    /// Where a *local-only* down goes: a monitor down from this node while
    /// every peer that answered the confirmation sees it up. Such a down never
    /// reaches the monitor's own channels (it is a problem near this node, not
    /// an outage); it goes to these channels, or, without this key, is only
    /// recorded (incident, timeline, the status page's "Not an outage").
    #[serde(default)]
    pub notify_unconfirmed: Option<Vec<String>>,
}

impl Default for Alerts {
    fn default() -> Self {
        Self {
            fail_threshold: default_threshold(),
            repeat_alert_secs: 0,
            alert_on_degraded: false,
            cert_expiry_days: default_cert_expiry_days(),
            domain_expiry_days: default_domain_expiry_days(),
            default_retention_days: default_retention_days(),
            group_window_secs: default_group_window(),
            push_alert_window_secs: default_push_alert_window(),
            channel_fail_threshold: default_channel_fail_threshold(),
            notify_unconfirmed: None,
        }
    }
}

/// Node-level dead-man / peer settings. A node with a `[health]` section emits an
/// outbound heartbeat to its peers (and any `heartbeat_url`) whenever it is
/// locally healthy, and exposes its view of those peers on `/healthz`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Health {
    /// This node's global identity. Peers refer to it by this id, and it is the
    /// key under which other nodes report this node in their `/healthz` `peers`.
    pub id: String,
    /// Cadence of outbound heartbeats, in seconds.
    #[serde(default = "default_health_interval")]
    pub interval_secs: u64,
    /// After startup, a peer that has never been seen is not alerted down until
    /// this many seconds have passed (it stays `unknown`).
    #[serde(default = "default_grace")]
    pub grace_secs: u64,
    /// Before alerting a peer down, poll the other peers' `/healthz`: if any still
    /// sees it up, treat it as a partition (`PeerLinkDegraded`) instead of an
    /// outage. With fewer than two other peers this is a no-op (honest mode).
    #[serde(default)]
    pub quorum: bool,
    /// An extra dead-man target (e.g. a healthchecks.io ping URL) pinged every
    /// `interval_secs` when locally healthy, alongside the peers' `ping_url`.
    /// The external target receives an empty POST, without the peer status digest.
    #[serde(default)]
    pub heartbeat_url: Option<Secret>,
    /// Multi-vantage confirmation: when a monitor confirms down locally, ask
    /// the peers to probe the same target from their side before alerting,
    /// and annotate the alert with the verdict ("confirmed from 3/3 vantage
    /// points" vs "seen UP by hora-b"). Per-monitor `confirm_with_peers`
    /// overrides this default either way. Strictly fail-open: peers being
    /// slow, broken or unreachable never blocks or suppresses the alert.
    #[serde(default)]
    pub confirm_with_peers: bool,
}

impl Health {
    #[must_use]
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_secs)
    }
}

fn default_title() -> String {
    "Status".to_owned()
}
fn default_history_days() -> u16 {
    90
}
fn default_bind() -> String {
    "127.0.0.1:8787".to_owned()
}
fn default_database_path() -> String {
    "hora.db".to_owned()
}
fn default_threshold() -> u32 {
    3
}
fn default_cert_expiry_days() -> u16 {
    14
}
fn default_domain_expiry_days() -> u16 {
    14
}
fn default_retention_days() -> u16 {
    90
}
fn default_rate_limit_refill() -> u64 {
    1
}
fn default_health_interval() -> u64 {
    60
}
fn default_grace() -> u64 {
    180
}
fn default_group_window() -> u64 {
    30
}
fn default_push_alert_window() -> u64 {
    300
}
fn default_channel_fail_threshold() -> u32 {
    3
}
fn default_rate_limit_burst() -> u32 {
    30
}

/// The configuration file path, from `$HORA_CONFIG` (default `./config.toml`).
#[must_use]
pub fn path() -> PathBuf {
    std::env::var_os("HORA_CONFIG").map_or_else(|| PathBuf::from("config.toml"), PathBuf::from)
}

/// Load the configuration from the default [`path`].
///
/// # Errors
///
/// Returns an error if the file cannot be read, parsed, or validated.
pub fn load() -> anyhow::Result<Config> {
    load_from(&path())
}

/// Load `path`, apply environment overrides, validate, and read the
/// `[page]` logos it names (relative to `path`).
///
/// # Errors
///
/// Returns an error if the file cannot be read, the TOML is invalid, a
/// monitor is misconfigured (empty/duplicate id, zero interval), or the logo
/// cannot be read.
pub fn load_from(path: &Path) -> anyhow::Result<Config> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    let config = parse(&raw)?;
    with_logos(config, path)
}

/// [`load_from`] through [`parse_lenient`]: faulty entries are dropped and
/// listed in [`Config::issues`]; a logo that cannot be read is one more issue.
///
/// # Errors
///
/// Only for what cannot be loaded in part (unreadable file, TOML syntax,
/// invalid global settings).
pub fn load_lenient_from(path: &Path) -> anyhow::Result<Config> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    let config = parse_lenient(&raw)?;
    let fallback = config.clone();
    match with_logos(config, path) {
        Ok(config) => Ok(config),
        Err(err) => {
            let mut config = fallback;
            config.page.logo = None;
            config.page.logo_dark = None;
            config.issues.push(ConfigIssue {
                item: "page.logo".to_owned(),
                reason: format!("{err:#}; no logo shown"),
            });
            Ok(config)
        }
    }
}

/// Read the `[page]` logos `config` names, relative to `path`.
fn with_logos(mut config: Config, path: &Path) -> anyhow::Result<Config> {
    let dir = path.parent().unwrap_or(Path::new(""));
    let page = &mut config.page;
    anyhow::ensure!(
        page.logo.is_some() || page.logo_dark.is_none(),
        "page.logo_dark needs page.logo, the light theme's"
    );
    if let Some(logo) = &page.logo {
        page.logo_image = Some(Logo::read(&dir.join(logo))?);
    }
    if let Some(logo) = &page.logo_dark {
        page.logo_dark_image = Some(Logo::read(&dir.join(logo))?);
    }
    Ok(config)
}

/// Parse, env-override and validate a configuration from TOML text.
///
/// # Errors
///
/// Returns an error if the TOML is invalid or a monitor is misconfigured
/// (empty/duplicate id, zero interval).
pub fn parse(toml_str: &str) -> anyhow::Result<Config> {
    parse_with_exec_dir(toml_str, std::env::var_os("HORA_EXEC_DIR").map(Into::into))
}

/// [`parse`] with an explicit exec directory instead of the `HORA_EXEC_DIR`
/// environment variable - the testable seam (tests must not mutate the
/// process environment).
///
/// # Errors
///
/// Returns an error if the TOML is invalid or a monitor is misconfigured.
pub fn parse_with_exec_dir(
    toml_str: &str,
    exec_dir: Option<std::path::PathBuf>,
) -> anyhow::Result<Config> {
    // Parse first, expand `${VAR}` afterwards - in string values only - so
    // secrets (channel tokens/URLs) can stay out of the file:
    // `webhook_url = "${OPS_DISCORD}"`. Expanding the raw text instead would
    // also hydrate comments (warning about unset variables in commented-out
    // examples) and let a syntax error echo an already-expanded secret back,
    // since TOML errors quote the offending line.
    let mut table: toml::Table = toml::from_str(toml_str).context("parsing config TOML")?;
    expand_env_table(&mut table);
    let mut config: Config = table.try_into().map_err(read_error)?;
    apply_env_overrides(&mut config);
    // Canonicalized once, here: the exec probe's escape check compares the
    // resolved plugin path against it, and must not re-resolve the directory
    // (blocking filesystem calls) on every tick. A directory that does not
    // resolve is kept as given; `validate_exec` rejects it if a monitor needs it.
    config.exec_dir = exec_dir.map(|dir| dir.canonicalize().unwrap_or(dir));
    validate(&config)?;
    Ok(config)
}

/// A deserialization error as the operator should read it. A monitor that
/// fails validation (which runs as each `[[monitors]]` table is read, see
/// [`Monitor`]) already names itself ("monitor api: ..."): that message is
/// the whole story, without serde's "in `monitors`" trailer. Anything else
/// (a type error, an unknown key) keeps the decoder's message and path.
fn read_error(err: toml::de::Error) -> anyhow::Error {
    if err.message().starts_with("monitor ") {
        anyhow::anyhow!(err.message().trim_end().to_owned())
    } else {
        anyhow::Error::new(err).context("reading config values")
    }
}

/// Infrastructure overrides honoured by the Docker image; secrets come from the
/// config (optionally via `${VAR}`), not from fixed variables.
fn apply_env_overrides(config: &mut Config) {
    if let Ok(bind) = std::env::var("HORA_BIND") {
        config.server.bind = bind;
    }
    if let Ok(path) = std::env::var("HORA_DATABASE_PATH") {
        config.server.database_path = path;
    }
}

/// Parse a cron schedule (`schedule`, `[digest]`). croner 4 rejects the
/// shorthand steps (`5/5`, `/10`) that 3.x accepted; `sloppy_ranges` keeps
/// configs written for earlier releases loading.
///
/// # Errors
///
/// Returns croner's parse error for an invalid pattern.
pub fn parse_cron(schedule: &str) -> Result<croner::Cron, croner::errors::CronError> {
    croner::parser::CronParser::builder()
        .sloppy_ranges(true)
        .build()
        .parse(schedule)
}
