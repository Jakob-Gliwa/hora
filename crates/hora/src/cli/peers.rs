//! `hora peers`: compare this node's monitors with each peer's.

use anyhow::Context as _;
use hora_core::config;

use super::{CliError, usage};

/// `hora peers diff`: compare this node's probeable monitors (kind + target)
/// with each peer's, over the authenticated `/api/peer/monitors` exchange.
/// `confirm_with_peers` only works on monitors both nodes know, and nothing
/// else verifies that alignment - this does, and exits non-zero on any drift
/// or unreachable peer so it can gate a config deploy.
pub(crate) async fn peers(args: &[String]) -> Result<(), CliError> {
    if args.first().map(String::as_str) != Some("diff") {
        return Err(usage("Usage: hora peers diff"));
    }
    let config = config::load_from(&config::path()).context("loading configuration")?;
    let Some(from) = config.health.as_ref().map(|health| health.id.clone()) else {
        return Err(CliError::Failed(
            "hora peers diff needs [health].id (the identity the peers authenticate).".to_owned(),
        ));
    };
    // Each askable peer paired with the URL that made it askable.
    let askable: Vec<(&hora_core::config::Peer, String)> = config
        .peers
        .iter()
        .filter_map(|peer| peer.monitors_url().map(|url| (peer, url)))
        .collect();
    if askable.is_empty() {
        return Err(CliError::Failed(
            "No askable peers: [[peers]] entries need a ping_url to derive the API origin."
                .to_owned(),
        ));
    }

    let local: std::collections::BTreeSet<(String, String)> = config
        .monitors
        .iter()
        .filter(|monitor| {
            !matches!(
                monitor.kind(),
                hora_core::config::Kind::Push
                    | hora_core::config::Kind::Exec
                    | hora_core::config::Kind::Mqtt
            )
        })
        .map(|monitor| {
            (
                monitor.kind().as_str().to_owned(),
                monitor.target().to_owned(),
            )
        })
        .collect();

    let client = hora_core::http::client(None).context("building HTTP client")?;
    let mut drift = false;
    println!("hora peers diff - {} local probeable monitors", local.len());
    for (peer, url) in askable {
        let answer = hora_core::mesh::vantage::fetch_peer_monitors(
            &client,
            &url,
            &from,
            peer.ping_token.as_ref().map(AsRef::as_ref),
        )
        .await;
        println!();
        drift |= print_peer_diff(&peer.name, &local, answer.as_ref());
    }
    if drift {
        println!();
        println!("Configs have drifted - multi-vantage confirmation only covers shared monitors.");
        return Err(CliError::Silent);
    }
    Ok(())
}

/// Print one peer's diff against the local monitor set; `true` means drift
/// (or an unreachable peer - a mesh you cannot ask is a mesh out of sync).
fn print_peer_diff(
    peer_name: &str,
    local: &std::collections::BTreeSet<(String, String)>,
    answer: Option<&hora_core::mesh::wire::PeerMonitors>,
) -> bool {
    let Some(answer) = answer else {
        println!("{peer_name}: UNREACHABLE (or refused the exchange)");
        return true;
    };
    let remote: std::collections::BTreeSet<(String, String)> = answer
        .monitors
        .iter()
        .map(|monitor| (monitor.kind.as_str().to_owned(), monitor.target.clone()))
        .collect();
    let missing: Vec<_> = local.difference(&remote).collect();
    let extra: Vec<_> = remote.difference(local).collect();
    if missing.is_empty() && extra.is_empty() {
        println!("{peer_name}: in sync ({} monitors)", remote.len());
        return false;
    }
    println!("{peer_name}: DRIFT");
    for (kind, target) in missing {
        println!("  missing there: {kind} {target}");
    }
    for (kind, target) in extra {
        println!("  only there:    {kind} {target}");
    }
    true
}
