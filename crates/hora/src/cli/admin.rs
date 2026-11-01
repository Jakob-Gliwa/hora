//! Operating the instance: config check, test alert, backup, digest preview,
//! doctor and the Uptime Kuma import.

use anyhow::Context as _;
use hora_core::config;

use super::{CliError, find_monitor, open_database, usage};

/// `hora import kuma <backup.json>`: convert an Uptime Kuma backup to Hora
/// TOML on stdout.
pub(crate) fn import_kuma(args: &[String]) -> Result<(), CliError> {
    let [kind, json_path, ..] = args else {
        return Err(usage("Usage: hora import kuma <backup.json>"));
    };
    if kind != "kuma" {
        return Err(usage("Usage: hora import kuma <backup.json>"));
    }
    let json_str =
        std::fs::read_to_string(json_path).with_context(|| format!("reading {json_path}"))?;
    let toml_out = hora_core::import::convert_kuma_to_hora(&json_str)?;
    println!("{toml_out}");
    Ok(())
}

/// `hora check`: validate the config and exit non-zero on error - meant for
/// CI and pre-deploy hooks.
pub(crate) fn check_config() -> Result<(), CliError> {
    let config_path = config::path();
    // How this environment mounts the file says nothing about its validity, so
    // a live-reload problem is a note, not a failure.
    for issue in hora_core::config_watch::live_reload_issues(&config_path) {
        println!("note: {issue}");
    }
    match config::load_lenient_from(&config_path) {
        Ok(config) if config.issues.is_empty() && !config.cannot_alert => {
            println!("{} is valid.", config_path.display());
            Ok(())
        }
        Ok(config) => {
            // Every issue, not just the first: the daemon would start without
            // these entries (and its configuration monitor would be down).
            for issue in &config.issues {
                println!("{issue}");
            }
            if config.cannot_alert {
                println!(
                    "no notification channel or no monitor could be loaded: Hora could not alert"
                );
            }
            Err(CliError::Failed(format!(
                "{} issue(s) in {}: Hora would start without the entries above",
                config.issues.len(),
                config_path.display()
            )))
        }
        Err(err) => Err(CliError::Failed(format!(
            "Configuration error (Hora cannot start with this file): {err:#}"
        ))),
    }
}

/// Send a test `Down` then `Recovered` through the real notification chain, so
/// an operator verifies delivery *before* the first real incident instead of
/// during it. Without an id every configured channel is exercised; with one,
/// the monitor's `notify` routing applies - testing exactly what would fire.
/// Failures surface as the notifiers' own per-channel warnings *and* as a
/// non-zero exit, so a CI pipeline can gate on the notification chain.
pub(crate) async fn test_alert(monitor_id: Option<&str>) -> Result<(), CliError> {
    let config_path = config::path();
    let config = config::load_from(&config_path).context("loading configuration")?;

    let (name, notify) = match monitor_id {
        None => ("Hora test".to_owned(), None),
        Some(id) => {
            let monitor = find_monitor(&config, id)?;
            (monitor.name.clone(), monitor.notify.clone())
        }
    };

    let client = hora_core::http::client(None).context("building HTTP client")?;
    let dispatcher = hora_core::notifications::build(&config, &client, None);
    let targeted: Vec<&str> = dispatcher
        .names()
        .filter(|channel| {
            notify
                .as_ref()
                .is_none_or(|only| only.iter().any(|name| name == channel))
        })
        .collect();
    if targeted.is_empty() {
        return Err(CliError::Failed(
            "No notification channel to test (none configured, or none routed).".to_owned(),
        ));
    }

    println!(
        "Sending a test alert (down + recovered) as {name:?} to: {}",
        targeted.join(", ")
    );
    let event = hora_core::notifications::Event::Down {
        monitor: &name,
        error: Some("test alert sent by `hora test-alert` - not a real incident"),
        cause: None,
        impacted: &[],
        vantage: None,
        event: None,
        local_only: false,
    };
    let mut failed = dispatcher.dispatch(event, notify.as_deref()).await;
    let failed_recovery = dispatcher
        .dispatch(
            hora_core::notifications::Event::Recovered {
                monitor: &name,
                local_only: false,
            },
            notify.as_deref(),
        )
        .await;
    // One verdict per channel: failing either delivery counts as failed.
    for channel in failed_recovery {
        if !failed.contains(&channel) {
            failed.push(channel);
        }
    }
    if failed.is_empty() {
        println!("Done. Every channel accepted both notifications.");
        Ok(())
    } else {
        Err(CliError::Failed(format!(
            "Delivery failed on: {} (the warnings above say why).",
            failed.join(", ")
        )))
    }
}

/// Snapshot the database to `dest` via `VACUUM INTO`: consistent and compacted,
/// safe while the daemon runs. Meant for cron ("a one-statement answer to 'what
/// if I lose a year of history?'").
pub(crate) async fn backup(dest: &str) -> Result<(), CliError> {
    let config_path = config::path();
    let config = config::load_from(&config_path).context("loading configuration")?;
    let source = &config.server.database_path;
    hora_core::db::backup_into(source, dest).await?;
    let size = std::fs::metadata(dest).map_or(0, |meta| meta.len());
    println!("Backed up {source} to {dest} ({} KiB).", size / 1024);
    Ok(())
}

/// Print the digest exactly as the `[digest]` task would send it - a dry run
/// to check the wording (and the data) without notifying anyone.
pub(crate) async fn digest_preview() -> Result<(), CliError> {
    let (config, store) = open_database().await?;
    let now = chrono::Utc::now().timestamp();
    let (period, summary) = hora_core::digest::build_summary(&store, &config, now).await?;
    println!("Hora digest ({period})");
    println!("{summary}");
    Ok(())
}

/// Diagnose the runtime environment against what the config needs: `hora
/// check` says the config is sound, `hora doctor` says the *host* can honour
/// it. Exits non-zero when a needed capability is missing.
pub(crate) async fn doctor() -> Result<(), CliError> {
    let config_path = config::path();
    let config = config::load_from(&config_path).context("loading configuration")?;
    println!(
        "hora doctor - {} ({} monitors)",
        config_path.display(),
        config.monitors.len()
    );
    println!();

    let findings = hora_core::doctor::run(&config).await;
    let mut failed = false;
    for finding in &findings {
        failed = failed || finding.status == hora_core::doctor::Status::Fail;
        println!(
            "  {:<10} {:<5} {}",
            finding.name,
            finding.status.label(),
            finding.detail
        );
    }
    if failed {
        println!();
        return Err(CliError::Failed(
            "Some capabilities the configuration needs are missing.".to_owned(),
        ));
    }
    Ok(())
}
