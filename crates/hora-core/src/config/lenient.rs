//! Lenient loading for the daemon: a faulty entry is dropped, named and
//! explained, instead of failing the whole configuration.
//!
//! Ten monitors with one typo should start nine, not none. [`parse`] (and
//! `hora check`) stay strict; the daemon loads through [`parse_lenient`], which
//! deserializes each monitor, channel, broker and maintenance window on its
//! own, removes what does not validate, then fixes the references that pointed
//! at removed entries: a maintenance window loses the missing monitor (and is
//! dropped when none is left - an empty list would mean "every monitor"), a
//! route loses the missing channel, an MQTT monitor goes with its broker.
//! Every removal becomes a [`ConfigIssue`], and the built-in monitor
//! [`CONFIG_MONITOR_ID`] reports them like any outage. Only what has no
//! partial form - a TOML syntax error, invalid global settings - still fails.
//!
//! [`parse`]: super::parse

use std::collections::HashSet;

use anyhow::Context;
use serde::Deserialize;

use super::{
    Channel, Config, Kind, Maintenance, Monitor, MonitorKind, expand_env_table, read_error,
};

/// Id of the built-in monitor that is down while the configuration has issues.
pub const CONFIG_MONITOR_ID: &str = "hora-config";
/// Id of the built-in self-check monitor (see `crate::self_check`).
pub const SELF_MONITOR_ID: &str = "hora-self";

/// Whether `id` is one of Hora's built-in monitors (fed by Hora, never by `/api/push`).
#[must_use]
pub fn is_builtin_monitor(id: &str) -> bool {
    id == CONFIG_MONITOR_ID || id == SELF_MONITOR_ID
}

/// One entry the daemon dropped or changed while loading, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigIssue {
    /// What it concerns, e.g. `monitor api` or `maintenance "Nightly"`.
    pub item: String,
    /// What was wrong and what was done about it.
    pub reason: String,
}

impl std::fmt::Display for ConfigIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.item, self.reason)
    }
}

/// `[config_monitor]` / `[self_monitor]`: how a built-in monitor shows and alerts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigMonitor {
    /// Display name (default "Hora configuration").
    #[serde(default)]
    pub name: Option<String>,
    /// Status-page group.
    #[serde(default)]
    pub group: Option<String>,
    /// Channels to alert; absent = every channel, like any monitor.
    #[serde(default)]
    pub notify: Option<Vec<String>>,
    /// Reminder period while issues persist; absent = `[alerts]` default.
    #[serde(default)]
    pub repeat_alert_secs: Option<u64>,
}

/// Lenient counterpart of [`super::parse`].
///
/// # Errors
///
/// Only for what cannot be loaded in part: a TOML syntax error, invalid
/// global settings (`[server]`, `[alerts]`, `[[peers]]`, ...).
pub fn parse_lenient(toml_str: &str) -> anyhow::Result<Config> {
    parse_lenient_with_exec_dir(toml_str, std::env::var_os("HORA_EXEC_DIR").map(Into::into))
}

/// [`parse_lenient`] with an explicit exec directory (the testable seam).
///
/// # Errors
///
/// See [`parse_lenient`].
pub fn parse_lenient_with_exec_dir(
    toml_str: &str,
    exec_dir: Option<std::path::PathBuf>,
) -> anyhow::Result<Config> {
    let mut table: toml::Table = toml::from_str(toml_str).context("parsing config TOML")?;
    expand_env_table(&mut table);
    let mut issues = Vec::new();

    // Each entry on its own: one that does not deserialize (type error,
    // unknown key, failed monitor validation) goes, the rest stays.
    let configured_monitors =
        keep_valid::<Monitor>(&mut table, "monitors", "id", "monitor", &mut issues);
    let configured_channels =
        keep_valid::<Channel>(&mut table, "channels", "name", "channel", &mut issues);
    keep_valid::<crate::mqtt::Broker>(&mut table, "brokers", "name", "broker", &mut issues);
    keep_valid::<Maintenance>(
        &mut table,
        "maintenance",
        "title",
        "maintenance",
        &mut issues,
    );

    let mut config: Config = table.try_into().map_err(read_error)?;
    super::apply_env_overrides(&mut config);
    config.exec_dir = exec_dir.map(|dir| dir.canonicalize().unwrap_or(dir));

    prune(&mut config, &mut issues);
    config.cannot_alert = (configured_channels > 0 && config.channels.is_empty())
        || (configured_monitors > 0 && config.monitors.is_empty());
    add_builtin(&mut config, &mut issues, CONFIG_MONITOR_ID)?;
    add_builtin(&mut config, &mut issues, SELF_MONITOR_ID)?;

    // Whatever the passes above do not foresee: drop the monitor an error
    // names and try again; anything else is a global error.
    let mut rounds = config.monitors.len() + 1;
    while let Err(err) = super::validate::validate(&config) {
        let message = format!("{err:#}");
        let named = config.monitors.iter().position(|monitor| {
            !is_builtin_monitor(&monitor.id) && names_monitor(&message, &monitor.id)
        });
        match named {
            Some(index) if rounds > 0 => {
                let monitor = config.monitors.remove(index);
                issue(
                    &mut issues,
                    format!("monitor {}", monitor.id),
                    format!("{message}; monitor not loaded"),
                );
                rounds -= 1;
            }
            _ => return Err(err),
        }
    }
    config.issues = issues;
    Ok(config)
}

fn issue(issues: &mut Vec<ConfigIssue>, item: String, reason: String) {
    issues.push(ConfigIssue { item, reason });
}

/// Whether `message` is about monitor `id` ("monitor api: ...", "... monitor api").
fn names_monitor(message: &str, id: &str) -> bool {
    message.split(['\n', ';']).any(|line| {
        let mut words = line.split_whitespace().peekable();
        while let Some(word) = words.next() {
            if word == "monitor"
                && words
                    .peek()
                    .is_some_and(|next| next.trim_end_matches([':', ',']) == id)
            {
                return true;
            }
        }
        false
    })
}

/// Keep the entries of the array `key` that deserialize as `T`; report the
/// others by their `label` field. Returns how many entries were configured.
fn keep_valid<T: serde::de::DeserializeOwned>(
    table: &mut toml::Table,
    key: &str,
    label: &str,
    kind: &str,
    issues: &mut Vec<ConfigIssue>,
) -> usize {
    let Some(toml::Value::Array(entries)) = table.remove(key) else {
        return 0;
    };
    let configured = entries.len();
    let mut kept = Vec::with_capacity(configured);
    for (index, entry) in entries.into_iter().enumerate() {
        match entry.clone().try_into::<T>() {
            Ok(_) => kept.push(entry),
            Err(err) => {
                let name = entry
                    .get(label)
                    .and_then(toml::Value::as_str)
                    .map_or_else(|| format!("#{}", index + 1), str::to_owned);
                let reason = err.message().trim_end().to_owned();
                // A monitor's own validation already names it ("monitor api: ...").
                let reason = reason
                    .strip_prefix(&format!("monitor {name}: "))
                    .map_or(reason.clone(), str::to_owned);
                issue(
                    issues,
                    format!("{kind} {name}"),
                    format!("{reason}; not loaded"),
                );
            }
        }
    }
    table.insert(key.to_owned(), toml::Value::Array(kept));
    configured
}

/// The cross-entry checks of the strict validation, as removals.
#[allow(clippy::too_many_lines)] // One pass per entry kind, in dependency order.
fn prune(config: &mut Config, issues: &mut Vec<ConfigIssue>) {
    // Channels: own options, unique non-empty names.
    let mut names = HashSet::new();
    config.channels.retain(|channel| {
        let problem = if let Channel::Pushover { options, .. } = channel
            && let Err(err) = options.validate()
        {
            Some(format!("{err:#}"))
        } else if channel.name().is_empty() {
            Some("channel name must not be empty".to_owned())
        } else if !names.insert(channel.name().to_owned()) {
            Some("duplicate channel name".to_owned())
        } else {
            None
        };
        problem.is_none_or(|reason| {
            issue(
                issues,
                format!("channel {}", channel.name()),
                format!("{reason}; not loaded"),
            );
            false
        })
    });
    let channels: HashSet<String> = config
        .channels
        .iter()
        .map(|c| c.name().to_owned())
        .collect();

    // Brokers: own settings, unique names and endpoints.
    let (mut names, mut endpoints) = (HashSet::new(), HashSet::new());
    config.brokers.retain(|broker| {
        let problem = match crate::mqtt::check_broker(broker) {
            Err(err) => Some(format!("{err:#}")),
            Ok(_) if names.contains(&broker.name) => Some("duplicate broker name".to_owned()),
            Ok(identity) if endpoints.contains(&identity) => {
                Some("duplicate MQTT endpoint/user; share one named broker".to_owned())
            }
            Ok(identity) => {
                names.insert(broker.name.clone());
                endpoints.insert(identity);
                None
            }
        };
        problem.is_none_or(|reason| {
            issue(
                issues,
                format!("broker {}", broker.name),
                format!("{reason}; not loaded"),
            );
            false
        })
    });

    // Monitors, each on its own.
    let auth = config.server.auth_token.is_some();
    let exec_ok = config.exec_dir.as_ref().is_some_and(|dir| dir.is_dir());
    let peer_ids: HashSet<String> = config
        .peers
        .iter()
        .map(|p| p.listen_id().to_owned())
        .collect();
    let mut ids = HashSet::new();
    let mut tokens = HashSet::new();
    let all_ids: HashSet<String> = config.monitors.iter().map(|m| m.id.clone()).collect();
    config.monitors.retain(|monitor| {
        let problem = monitor_problem(
            monitor,
            &mut ids,
            &mut tokens,
            &all_ids,
            &peer_ids,
            &names,
            auth,
            exec_ok,
        );
        problem.is_none_or(|reason| {
            issue(
                issues,
                format!("monitor {}", monitor.id),
                format!("{reason}; not loaded"),
            );
            false
        })
    });

    // References to what is gone.
    let ids: HashSet<String> = config.monitors.iter().map(|m| m.id.clone()).collect();
    for monitor in &mut config.monitors {
        let owner = format!("monitor {}", monitor.id);
        drop_routes(&owner, &mut monitor.notify, &channels, issues);
        if let Some(deps) = &mut monitor.depends_on {
            deps.retain(|dep| {
                ids.contains(dep) || {
                    issue(
                        issues,
                        owner.clone(),
                        format!("depends_on {dep:?} names no loaded monitor; dependency ignored"),
                    );
                    false
                }
            });
        }
    }
    if crate::topology::validate_dag(&config.monitors).is_err() {
        for monitor in &mut config.monitors {
            if monitor
                .depends_on
                .take()
                .is_some_and(|deps| !deps.is_empty())
            {
                issue(
                    issues,
                    format!("monitor {}", monitor.id),
                    "part of a dependency cycle; depends_on ignored".to_owned(),
                );
            }
        }
    }
    config.maintenance.retain_mut(|window| {
        let item = format!("maintenance {:?}", window.title);
        if let Err(err) = window.validate() {
            issue(issues, item, format!("{err:#}; not loaded"));
            return false;
        }
        if window.monitors.is_empty() {
            return true; // every monitor, by design
        }
        let (kept, missing): (Vec<String>, Vec<String>) =
            window.monitors.drain(..).partition(|id| ids.contains(id));
        window.monitors = kept;
        if window.monitors.is_empty() {
            // An emptied list would silence every monitor: drop the window instead.
            issue(
                issues,
                item,
                format!(
                    "none of its monitors is loaded ({}); window not loaded",
                    missing.join(", ")
                ),
            );
            return false;
        }
        for id in missing {
            issue(
                issues,
                item.clone(),
                format!("monitor {id} is not loaded; removed from the window"),
            );
        }
        true
    });
    let groups: HashSet<Option<String>> = config.monitors.iter().map(|m| m.group.clone()).collect();
    config.server.group_tokens.retain(|group, _| {
        groups.contains(&Some(group.clone())) || {
            issue(
                issues,
                format!("server.group_tokens[{group:?}]"),
                "no loaded monitor belongs to this group; token ignored".to_owned(),
            );
            false
        }
    });
    drop_routes("updates", &mut config.updates.notify, &channels, issues);
    drop_routes(
        "alerts.notify_unconfirmed",
        &mut config.alerts.notify_unconfirmed,
        &channels,
        issues,
    );
    if let Some(digest) = &mut config.digest {
        drop_routes("digest", &mut digest.notify, &channels, issues);
        if let Err(err) = super::parse_cron(&digest.schedule) {
            issue(
                issues,
                "digest".to_owned(),
                format!(
                    "invalid schedule {:?}: {err}; digest not loaded",
                    digest.schedule
                ),
            );
            config.digest = None;
        }
    }
    for peer in &mut config.peers {
        drop_routes(
            &format!("peer {}", peer.id),
            &mut peer.notify,
            &channels,
            issues,
        );
    }
    if let Some(section) = &mut config.config_monitor {
        drop_routes("config_monitor", &mut section.notify, &channels, issues);
    }
    if let Some(section) = &mut config.self_monitor {
        drop_routes("self_monitor", &mut section.notify, &channels, issues);
    }
}

/// Why a single monitor cannot run, judged against the ones kept before it.
#[allow(clippy::too_many_arguments)]
fn monitor_problem(
    monitor: &Monitor,
    ids: &mut HashSet<String>,
    tokens: &mut HashSet<String>,
    all_ids: &HashSet<String>,
    peer_ids: &HashSet<String>,
    brokers: &HashSet<String>,
    auth: bool,
    exec_ok: bool,
) -> Option<String> {
    if is_builtin_monitor(&monitor.id) {
        return Some(format!(
            "id {:?} is reserved for a built-in monitor",
            monitor.id
        ));
    }
    if monitor.id.is_empty() || !super::validate::is_url_safe_id(&monitor.id) {
        return Some("monitor id must be non-empty and alphanumeric, '-' or '_'".to_owned());
    }
    if !ids.insert(monitor.id.clone()) {
        return Some("duplicate monitor id (the first one is loaded)".to_owned());
    }
    for (label, secs) in [
        ("interval_secs", monitor.interval_secs),
        ("timeout_secs", monitor.timeout_secs),
    ] {
        if let Err(err) = super::validate::validate_period(label, secs) {
            return Some(format!("{err:#}"));
        }
    }
    if !monitor.public && !auth {
        return Some("public = false requires server.auth_token to be set".to_owned());
    }
    if monitor
        .push_token
        .as_ref()
        .is_some_and(super::Secret::is_empty)
    {
        return Some("push_token must not be empty".to_owned());
    }
    if let MonitorKind::Mqtt(spec) = &monitor.spec
        && !brokers.contains(&spec.broker)
    {
        return Some(format!("MQTT broker {:?} is not loaded", spec.broker));
    }
    if monitor.confirm_with_peers == Some(true) && matches!(monitor.kind(), Kind::Push | Kind::Exec)
    {
        return Some("confirm_with_peers needs a network probe".to_owned());
    }
    if monitor.kind() == Kind::Exec && !exec_ok {
        return Some("exec monitors require HORA_EXEC_DIR to name a directory".to_owned());
    }
    if let MonitorKind::Push(push) = &monitor.spec
        && push.kuma_compatible
    {
        let token = monitor
            .push_token
            .as_ref()
            .map(AsRef::as_ref)
            .unwrap_or_default();
        if !super::validate::is_url_safe_id(token) {
            return Some("Kuma push tokens must be URL-safe".to_owned());
        }
        if all_ids.contains(token) || peer_ids.contains(token) {
            return Some("Kuma push token must not collide with a monitor or peer ID".to_owned());
        }
        if !tokens.insert(token.to_owned()) {
            return Some("Kuma push token already used by another monitor".to_owned());
        }
    }
    None
}

/// Remove routes naming channels that are not loaded.
fn drop_routes(
    owner: &str,
    routes: &mut Option<Vec<String>>,
    channels: &HashSet<String>,
    issues: &mut Vec<ConfigIssue>,
) {
    if let Some(routes) = routes {
        routes.retain(|route| {
            channels.contains(route) || {
                issue(
                    issues,
                    owner.to_owned(),
                    format!("notify channel {route:?} is not loaded; route removed"),
                );
                false
            }
        });
    }
}
/// Append a built-in monitor: a push monitor fed by the daemon itself (never
/// by `/api/push`), down while it has something to report.
fn add_builtin(config: &mut Config, issues: &mut Vec<ConfigIssue>, id: &str) -> anyhow::Result<()> {
    let config_monitor = id == CONFIG_MONITOR_ID;
    let (key, default_name) = if config_monitor {
        ("config_monitor", "Hora configuration")
    } else {
        ("self_monitor", "Hora self-check")
    };
    let section = if config_monitor {
        config.config_monitor.clone()
    } else {
        config.self_monitor.clone()
    }
    .unwrap_or_default();
    let mut table = toml::Table::new();
    table.insert("id".into(), id.into());
    table.insert(
        "name".into(),
        section
            .name
            .unwrap_or_else(|| default_name.to_owned())
            .into(),
    );
    table.insert("kind".into(), "push".into());
    table.insert(
        "interval_secs".into(),
        toml::Value::Integer(i64::try_from(super::validate::MAX_PERIOD_SECS)?),
    );
    table.insert("fail_threshold".into(), toml::Value::Integer(1));
    table.insert(
        "public".into(),
        toml::Value::Boolean(config.server.auth_token.is_none()),
    );
    if let Some(group) = section.group {
        table.insert("group".into(), group.into());
    }
    if let Some(notify) = section.notify {
        table.insert(
            "notify".into(),
            toml::Value::Array(notify.into_iter().map(Into::into).collect()),
        );
    }
    if let Some(secs) = section.repeat_alert_secs {
        table.insert(
            "repeat_alert_secs".into(),
            toml::Value::Integer(i64::try_from(secs).unwrap_or(i64::MAX)),
        );
    }
    match toml::Value::Table(table).try_into::<Monitor>() {
        Ok(monitor) => config.monitors.push(monitor),
        Err(err) => {
            issue(
                issues,
                key.to_owned(),
                format!("{}; defaults used", err.message().trim_end()),
            );
            if config_monitor {
                config.config_monitor = None;
            } else {
                config.self_monitor = None;
            }
            return add_builtin(config, issues, id);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = r"
        [page]
        [server]
    ";

    fn load(body: &str) -> Config {
        parse_lenient_with_exec_dir(&format!("{HEAD}{body}"), None).expect("lenient load")
    }

    fn ids(config: &Config) -> Vec<&str> {
        config.monitors.iter().map(|m| m.id.as_str()).collect()
    }

    fn has_issue(config: &Config, item: &str, needle: &str) -> bool {
        config
            .issues
            .iter()
            .any(|issue| issue.item == item && issue.reason.contains(needle))
    }

    fn http(id: &str) -> String {
        format!(
            "[[monitors]]\nid = \"{id}\"\nname = \"{id}\"\ntarget = \"https://example.com\"\ninterval_secs = 60\n"
        )
    }

    #[test]
    fn a_faulty_monitor_is_dropped_and_the_rest_starts() {
        let config = load(&format!(
            "{}{}[[monitors]]\nid = \"bad\"\nname = \"Bad\"\ntarget = \"https://example.com\"\ninterval_secs = 0\n",
            http("a"),
            http("b")
        ));
        assert_eq!(ids(&config), ["a", "b", CONFIG_MONITOR_ID, SELF_MONITOR_ID]);
        assert!(
            has_issue(&config, "monitor bad", "not loaded"),
            "{:?}",
            config.issues
        );
        assert!(!config.cannot_alert);
    }

    #[test]
    fn an_unknown_key_drops_only_its_monitor() {
        let config = load(&format!(
            "{}[[monitors]]\nid = \"typo\"\nname = \"T\"\ntarget = \"https://example.com\"\nintervall_secs = 60\n",
            http("a")
        ));
        assert_eq!(ids(&config), ["a", CONFIG_MONITOR_ID, SELF_MONITOR_ID]);
        assert!(
            has_issue(&config, "monitor typo", "not loaded"),
            "{:?}",
            config.issues
        );
    }

    #[test]
    fn duplicate_ids_keep_the_first() {
        let config = load(&format!("{}{}", http("a"), http("a")));
        assert_eq!(ids(&config), ["a", CONFIG_MONITOR_ID, SELF_MONITOR_ID]);
        assert!(has_issue(&config, "monitor a", "duplicate"));
    }

    #[test]
    fn maintenance_loses_missing_monitors_and_an_emptied_window_is_dropped() {
        let config = load(&format!(
            "{}\
             [[maintenance]]\ntitle = \"Mixed\"\ncron = \"0 0 * * *\"\nduration_secs = 900\nmonitors = [\"a\", \"gone\"]\n\
             [[maintenance]]\ntitle = \"Only gone\"\ncron = \"0 0 * * *\"\nduration_secs = 900\nmonitors = [\"gone\"]\n\
             [[maintenance]]\ntitle = \"Everything\"\ncron = \"0 0 * * *\"\nduration_secs = 900\nmonitors = []\n",
            http("a")
        ));
        let titles: Vec<_> = config
            .maintenance
            .iter()
            .map(|w| w.title.as_str())
            .collect();
        assert_eq!(titles, ["Mixed", "Everything"]);
        assert_eq!(config.maintenance[0].monitors, ["a"]);
        assert!(
            config.maintenance[1].monitors.is_empty(),
            "an explicit empty list still means every monitor"
        );
        assert!(has_issue(
            &config,
            "maintenance \"Mixed\"",
            "gone is not loaded"
        ));
        assert!(has_issue(
            &config,
            "maintenance \"Only gone\"",
            "window not loaded"
        ));
    }

    #[test]
    fn a_faulty_channel_is_dropped_and_routes_to_it_removed() {
        let config = load(&format!(
            "[[channels]]\nname = \"ok\"\ntype = \"webhook\"\nurl = \"https://hooks.example.com/x\"\n\
             [[channels]]\nname = \"loud\"\ntype = \"pushover\"\ntoken = \"t\"\nuser = \"u\"\npriority = 5\n\
             {}notify = [\"ok\", \"loud\"]\n",
            http("a")
        ));
        let names: Vec<_> = config.channels.iter().map(Channel::name).collect();
        assert_eq!(names, ["ok"]);
        assert_eq!(
            config.monitors[0].notify.as_deref(),
            Some(&["ok".to_owned()][..])
        );
        assert!(
            has_issue(&config, "monitor a", "\"loud\" is not loaded"),
            "{:?}",
            config.issues
        );
        assert!(!config.cannot_alert);
    }

    #[test]
    fn losing_every_channel_means_hora_cannot_alert() {
        let config = load(&format!(
            "[[channels]]\nname = \"loud\"\ntype = \"pushover\"\ntoken = \"t\"\nuser = \"u\"\npriority = 5\n{}",
            http("a")
        ));
        assert_eq!(config.channels.len(), 0);
        assert!(config.cannot_alert);
    }

    #[test]
    fn a_faulty_broker_takes_its_mqtt_monitors_along() {
        let config = load(&format!(
            "[[brokers]]\nname = \"home\"\nurl = \"http://broker.example\"\n\
             [[monitors]]\nid = \"sensor\"\nname = \"Sensor\"\nkind = \"mqtt\"\nbroker = \"home\"\ntopic = \"t/state\"\nkeyword = \"online\"\ninterval_secs = 60\n{}",
            http("a")
        ));
        assert_eq!(config.brokers.len(), 0);
        assert_eq!(ids(&config), ["a", CONFIG_MONITOR_ID, SELF_MONITOR_ID]);
        assert!(has_issue(&config, "broker home", "not loaded"));
        assert!(has_issue(&config, "monitor sensor", "is not loaded"));
    }

    #[test]
    fn a_missing_dependency_is_ignored_not_fatal() {
        let config = load(&format!("{}depends_on = [\"gone\"]\n", http("a")));
        assert_eq!(config.monitors[0].depends_on.as_deref(), Some(&[][..]));
        assert!(has_issue(&config, "monitor a", "depends_on \"gone\""));
    }

    #[test]
    fn the_config_monitor_is_built_in_and_its_id_reserved() {
        let config = load(&format!(
            "[config_monitor]\nname = \"Hora: Konfiguration\"\ngroup = \"Watchdog\"\n{}{}",
            http("a"),
            http(CONFIG_MONITOR_ID)
        ));
        let config_self = load(&format!(
            "[self_monitor]\ngroup = \"Watchdog\"\n{}{}",
            http("a"),
            http(SELF_MONITOR_ID)
        ));
        assert_eq!(
            config_self
                .find_monitor(SELF_MONITOR_ID)
                .and_then(|m| m.group.as_deref()),
            Some("Watchdog")
        );
        assert!(has_issue(
            &config_self,
            &format!("monitor {SELF_MONITOR_ID}"),
            "reserved"
        ));
        let builtin = config
            .find_monitor(CONFIG_MONITOR_ID)
            .expect("config monitor");
        assert_eq!(builtin.name, "Hora: Konfiguration");
        assert_eq!(builtin.group.as_deref(), Some("Watchdog"));
        assert_eq!(builtin.kind(), Kind::Push);
        assert_eq!(builtin.fail_threshold, Some(1));
        assert_eq!(
            config
                .monitors
                .iter()
                .filter(|m| m.id == CONFIG_MONITOR_ID)
                .count(),
            1
        );
        assert!(has_issue(
            &config,
            &format!("monitor {CONFIG_MONITOR_ID}"),
            "reserved"
        ));
    }

    #[test]
    fn a_clean_file_has_no_issues() {
        let config = load(&http("a"));
        assert_eq!(config.issues.len(), 0, "{:?}", config.issues);
        assert_eq!(ids(&config), ["a", CONFIG_MONITOR_ID, SELF_MONITOR_ID]);
    }

    #[test]
    fn what_has_no_partial_form_still_fails() {
        assert!(
            parse_lenient_with_exec_dir("[[monitors]\nid = \"a\"", None).is_err(),
            "TOML syntax"
        );
        assert!(
            parse_lenient_with_exec_dir("[page]\n[server]\nbind = 5\n", None).is_err(),
            "global type error"
        );
    }

    #[test]
    fn the_strict_parse_is_unchanged() {
        let toml = format!(
            "{HEAD}{}[[monitors]]\nid = \"bad\"\nname = \"Bad\"\ntarget = \"https://example.com\"\ninterval_secs = 0\n",
            http("a")
        );
        assert!(super::super::parse(&toml).is_err());
    }
}
