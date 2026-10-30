//! The validation pass run once at load: everything serde's types cannot
//! express (cross-references, bounds, mutually exclusive fields).

mod monitor;

use std::collections::HashSet;

use super::{Channel, Config, Kind, Secret, parse_cron};
pub(crate) use monitor::monitor_from_raw;

/// A configured access token must be non-empty: an empty one (often the result
/// of an unset `${VAR}` expanding to "") would authorize a blank `?token=`, since
/// the constant-time compare treats `"" == ""` as a match. Short-but-set tokens
/// are the operator's call, so they only warn.
fn validate_token(label: &str, token: Option<&Secret>) -> anyhow::Result<()> {
    if let Some(token) = token {
        anyhow::ensure!(!token.is_empty(), "{label} must not be empty");
        if token.as_ref().len() < 16 {
            tracing::warn!("{label} is short (under 16 chars); prefer a long random token");
        }
    }
    Ok(())
}

/// Upper bound on every configured period (`interval_secs`, `timeout_secs`,
/// `expect_every_secs`): 30 days. Nothing sensible is slower, and a typo'd
/// huge value would overflow `Instant` arithmetic in the tick timers - a panic
/// that silently stops the task instead of a load error.
const MAX_PERIOD_SECS: u64 = 30 * 24 * 3600;

/// A period in seconds must be positive and at most [`MAX_PERIOD_SECS`].
fn validate_period(label: &str, secs: u64) -> anyhow::Result<()> {
    anyhow::ensure!(secs > 0, "{label} must be > 0");
    anyhow::ensure!(
        secs <= MAX_PERIOD_SECS,
        "{label} must be at most {MAX_PERIOD_SECS} (30 days)"
    );
    Ok(())
}

/// Ids that appear in URLs (`/api/badge/{id}`, `/api/push/{id}`) must stay
/// URL-safe.
fn is_url_safe_id(id: &str) -> bool {
    id.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Every `notify` route must name a configured channel.
fn validate_routes(
    owner: &str,
    routes: Option<&[String]>,
    channel_names: &HashSet<&str>,
) -> anyhow::Result<()> {
    for route in routes.unwrap_or_default() {
        anyhow::ensure!(
            channel_names.contains(route.as_str()),
            "{owner}: notify references unknown channel {route:?}"
        );
    }
    Ok(())
}

pub(super) fn validate(config: &Config) -> anyhow::Result<()> {
    validate_token("server.auth_token", config.server.auth_token.as_ref())?;
    if config.alerts.repeat_alert_secs != 0 {
        validate_period("alerts.repeat_alert_secs", config.alerts.repeat_alert_secs)?;
    }
    for (group, token) in &config.server.group_tokens {
        validate_token(&format!("server.group_tokens[{group:?}]"), Some(token))?;
        // A token for a group no monitor belongs to guards nothing - that is
        // a typo'd group name, not a future plan.
        anyhow::ensure!(
            config
                .monitors
                .iter()
                .any(|monitor| monitor.group.as_deref() == Some(group)),
            "server.group_tokens: no monitor belongs to group {group:?}"
        );
    }
    let channel_names = validate_channels(config)?;

    for window in &config.maintenance {
        window.validate()?;
        for id in &window.monitors {
            anyhow::ensure!(
                config.find_monitor(id).is_some(),
                "maintenance references unknown monitor {id}"
            );
        }
    }

    let mut seen = HashSet::new();
    for monitor in &config.monitors {
        anyhow::ensure!(!monitor.id.is_empty(), "monitor id must not be empty");
        // The id appears in URLs (`/api/badge/{id}`, `/api/push/{id}`), so keep it
        // URL-safe.
        anyhow::ensure!(
            is_url_safe_id(&monitor.id),
            "monitor id {:?} must be alphanumeric, '-' or '_'",
            monitor.id
        );
        anyhow::ensure!(
            seen.insert(monitor.id.as_str()),
            "duplicate monitor id: {}",
            monitor.id
        );
        validate_period(
            &format!("monitor {} interval_secs", monitor.id),
            monitor.interval_secs,
        )?;
        validate_period(
            &format!("monitor {} timeout_secs", monitor.id),
            monitor.timeout_secs,
        )?;
        // A probe outliving its interval delays the next tick (the scheduler
        // never overlaps probes), so the effective cadence is the timeout,
        // not the interval. Push monitors probe nothing; their timeout is inert.
        if monitor.kind() != Kind::Push && monitor.timeout_secs > monitor.interval_secs {
            tracing::warn!(
                "monitor {}: timeout_secs ({}) exceeds interval_secs ({}) - a hung probe \
                 delays the next check by up to the timeout",
                monitor.id,
                monitor.timeout_secs,
                monitor.interval_secs
            );
        }
        // A private monitor without a configured token would be visible to no
        // one at all - fail fast rather than silently hide it. (Emptiness is
        // already rejected by validate_token above, so presence is enough.)
        anyhow::ensure!(
            monitor.public || config.server.auth_token.is_some(),
            "monitor {}: public = false requires server.auth_token to be set",
            monitor.id
        );
        // Anonymous viewers never see a private monitor, so the opt-in is inert;
        // warn rather than fail, since `public` may be toggled temporarily.
        if monitor.public_error_detail && !monitor.public {
            tracing::warn!(
                "monitor {}: public_error_detail has no effect on a private monitor",
                monitor.id
            );
        }
        validate_token(
            &format!("monitor {}: push_token", monitor.id),
            monitor.push_token.as_ref(),
        )?;
        // Without a token the id alone authorizes /api/push/{id}, and ids are
        // not secrets (a public monitor's id is served on the page and API):
        // anyone could forge heartbeats to mask an outage. Warn, don't fail -
        // a bare id may be acceptable on a private network.
        if monitor.kind() == Kind::Push && monitor.push_token.is_none() {
            tracing::warn!(
                "monitor {}: push monitor has no push_token - its id appears on the \
                 status page/API, so anyone who can reach /api/push can forge heartbeats",
                monitor.id
            );
        }
        validate_routes(
            &format!("monitor {}", monitor.id),
            monitor.notify.as_deref(),
            &channel_names,
        )?;
    }

    let mut push_tokens = HashSet::new();
    for monitor in &config.monitors {
        if matches!(&monitor.spec, super::MonitorKind::Push(push) if push.kuma_compatible) {
            let token = monitor
                .push_token
                .as_ref()
                .expect("validated push token")
                .as_ref();
            anyhow::ensure!(is_url_safe_id(token), "Kuma push tokens must be URL-safe");
            anyhow::ensure!(push_tokens.insert(token), "Kuma push tokens must be unique");
            anyhow::ensure!(
                !seen.contains(token) && !config.peers.iter().any(|p| p.listen_id() == token),
                "Kuma push token must not collide with a monitor or peer ID"
            );
        }
    }
    crate::topology::validate_dag(&config.monitors)?;

    validate_peers(config, &seen, &channel_names)?;

    validate_confirm(config)?;
    validate_exec(config)?;
    validate_digest(config, &channel_names)?;
    validate_routes("updates", config.updates.notify.as_deref(), &channel_names)?;
    validate_routes(
        "alerts.notify_unconfirmed",
        config.alerts.notify_unconfirmed.as_deref(),
        &channel_names,
    )?;
    Ok(())
}

/// Multi-vantage confirmation needs peers that can actually be asked: a
/// `[health]` section and at least one peer with a `ping_url` (its API origin).
/// A flag that can never confirm anything is a config mistake, not a plan.
fn validate_confirm(config: &Config) -> anyhow::Result<()> {
    let confirmable_peers = config.peers.iter().any(|peer| peer.probe_url().is_some());
    let confirm_requested = config
        .health
        .as_ref()
        .is_some_and(|health| health.confirm_with_peers)
        || config
            .monitors
            .iter()
            .any(|monitor| monitor.confirm_with_peers == Some(true));
    if confirm_requested {
        anyhow::ensure!(
            config.health.is_some(),
            "confirm_with_peers requires a [health] section"
        );
        anyhow::ensure!(
            confirmable_peers,
            "confirm_with_peers requires at least one peer with a ping_url \
             (its API origin is where probe requests go)"
        );
    }
    for monitor in &config.monitors {
        anyhow::ensure!(
            !(monitor.confirm_with_peers == Some(true)
                && matches!(monitor.kind(), Kind::Push | Kind::Exec)),
            "monitor {}: confirm_with_peers needs a network probe (push monitors \
             have no target, and exec checks are local to this host)",
            monitor.id
        );
    }
    Ok(())
}

/// Exec probes are gated on `HORA_EXEC_DIR` (deployment-level consent): a
/// config declaring one without the variable - or with a directory that does
/// not exist - fails at load, not at the first probe.
fn validate_exec(config: &Config) -> anyhow::Result<()> {
    if !config.monitors.iter().any(|m| m.kind() == Kind::Exec) {
        return Ok(());
    }
    let Some(dir) = &config.exec_dir else {
        anyhow::bail!(
            "exec monitors require the HORA_EXEC_DIR environment variable \
             (the directory whose executables they may run)"
        );
    };
    anyhow::ensure!(
        dir.is_dir(),
        "HORA_EXEC_DIR {} is not a directory",
        dir.display()
    );
    Ok(())
}

/// Digest: the cron must parse at load (not at the first missed send), and
/// routes must name real channels, like a monitor's `notify`.
fn validate_digest(
    config: &Config,
    channel_names: &std::collections::HashSet<&str>,
) -> anyhow::Result<()> {
    if let Some(digest) = &config.digest {
        parse_cron(&digest.schedule).map_err(|err| {
            anyhow::anyhow!("digest: invalid schedule {:?}: {err}", digest.schedule)
        })?;
        validate_routes("digest", digest.notify.as_deref(), channel_names)?;
    }
    Ok(())
}

/// Validate the notification channels (unique non-empty names, cleartext-URL
/// warnings) and return the set of valid channel names for route checks.
fn validate_channels(config: &Config) -> anyhow::Result<HashSet<&str>> {
    let mut channel_names = HashSet::new();
    for channel in &config.channels {
        if let Channel::Pushover { options, .. } = channel {
            options.validate()?;
        }
        anyhow::ensure!(!channel.name().is_empty(), "channel name must not be empty");
        anyhow::ensure!(
            channel_names.insert(channel.name()),
            "duplicate channel name: {}",
            channel.name()
        );
        // These channels send credentials over the configured URL (in the URL
        // itself, or - for Matrix - in a header); warn on cleartext http.
        let url = match channel {
            Channel::Discord { webhook_url, .. } | Channel::Slack { webhook_url, .. } => {
                Some(webhook_url.as_ref())
            }
            Channel::Webhook { url, .. }
            | Channel::Ntfy { url, .. }
            | Channel::Gotify { url, .. } => Some(url.as_ref()),
            Channel::Matrix { homeserver, .. } => Some(homeserver.as_str()),
            Channel::Telegram { .. }
            | Channel::FreeMobile { .. }
            | Channel::Email { .. }
            | Channel::Pushover { .. } => None,
        };
        if url.is_some_and(|url| url.starts_with("http://")) {
            tracing::warn!(
                "channel {}: URL uses http:// - credentials are sent in cleartext, use https",
                channel.name()
            );
        }
    }
    Ok(channel_names)
}

/// Validate the `[health]` section and the `[[peers]]` mesh: peers require a
/// health section; a watched peer's `listen_id` must be URL-safe and clash with
/// neither a monitor nor another peer; and every peer must do something (ping out,
/// be watched, or both).
fn validate_peers(
    config: &Config,
    monitor_ids: &HashSet<&str>,
    channel_names: &HashSet<&str>,
) -> anyhow::Result<()> {
    if let Some(health) = &config.health {
        anyhow::ensure!(!health.id.is_empty(), "health.id must not be empty");
        validate_period("health.interval_secs", health.interval_secs)?;
    }
    anyhow::ensure!(
        config.peers.is_empty() || config.health.is_some(),
        "[[peers]] require a [health] section (it sets this node's id and heartbeat interval)"
    );

    let mut peer_ids: HashSet<&str> = HashSet::new();
    let mut listen_ids: HashSet<&str> = HashSet::new();
    for peer in &config.peers {
        anyhow::ensure!(!peer.id.is_empty(), "peer id must not be empty");
        anyhow::ensure!(
            peer_ids.insert(peer.id.as_str()),
            "duplicate peer id: {}",
            peer.id
        );
        anyhow::ensure!(
            peer.ping_url.is_some() || peer.is_watched(),
            "peer {}: set ping_url (to heartbeat it) and/or expect_every_secs (to watch it)",
            peer.id
        );
        validate_token(
            &format!("peer {}: listen_token", peer.id),
            peer.listen_token.as_ref(),
        )?;
        validate_token(
            &format!("peer {}: ping_token", peer.id),
            peer.ping_token.as_ref(),
        )?;
        if let Some(every) = peer.expect_every_secs {
            validate_period(&format!("peer {}: expect_every_secs", peer.id), every)?;
            // Same reasoning as the push-monitor warning: peer ids are exposed
            // on the unauthenticated /healthz (witnesses need them), so an
            // unprotected listen id lets anyone forge the peer's heartbeats.
            if peer.listen_token.is_none() {
                tracing::warn!(
                    "peer {}: watched without a listen_token - its id appears in /healthz, \
                     so anyone who can reach /api/push can forge its heartbeats",
                    peer.id
                );
            }
            // The id appears in `/api/push/{id}`, so keep it URL-safe.
            let listen_id = peer.listen_id();
            anyhow::ensure!(
                is_url_safe_id(listen_id),
                "peer {}: listen_id {listen_id:?} must be alphanumeric, '-' or '_'",
                peer.id
            );
            anyhow::ensure!(
                !monitor_ids.contains(listen_id),
                "peer {}: listen_id {listen_id:?} clashes with a monitor id",
                peer.id
            );
            anyhow::ensure!(
                listen_ids.insert(listen_id),
                "peer {}: duplicate listen_id {listen_id:?}",
                peer.id
            );
        }
        validate_routes(
            &format!("peer {}", peer.id),
            peer.notify.as_deref(),
            channel_names,
        )?;
        // A push token sent over cleartext http to the peer would leak in transit.
        if let (Some(url), Some(_token)) = (&peer.ping_url, &peer.ping_token)
            && url.as_ref().starts_with("http://")
        {
            tracing::warn!(
                "peer {}: ping_url uses http:// - the push token is sent in cleartext, use https",
                peer.id
            );
        }
    }
    Ok(())
}
