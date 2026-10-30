//! Producer writes: push heartbeats and pushed alerts.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::Deserialize;

use hora_core::config::{Kind, Monitor, MonitorKind};
use hora_core::db::{self};
use hora_core::notifications::{AlertSeverity, Event};
use hora_core::status::CheckStatus;

use crate::auth::{QueryTokenAuth, Viewer, ct_eq, push_token};
use crate::error::AppError;
use crate::flood;
use crate::{AppState, MAX_ALERT_DEDUP_CHARS, MAX_ALERT_TAG_CHARS, MAX_ALERT_TAGS};
use hora_core::{MAX_ALERT_TITLE_CHARS, MAX_PUSH_MSG_CHARS};

#[derive(Debug, Deserialize)]
pub(crate) struct PushQuery {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    msg: Option<String>,
    #[serde(default)]
    ping: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/push/{id}",
    params(
        ("id" = String, Path, description = "Push monitor id"),
        ("token" = Option<String>, Query, deprecated, description = "Push token, if the monitor sets one; deprecated (answered with a Deprecation header): use X-Push-Token"),
        ("status" = Option<String>, Query, description = "up (default), down or degraded"),
        ("msg" = Option<String>, Query, description = "Optional detail recorded with the heartbeat"),
        ("ping" = Option<i64>, Query, description = "Optional round-trip latency in ms (>= 0)")
    ),
    security((), ("push_token" = [])),
    responses(
        (status = 200, description = "Heartbeat recorded"),
        (status = 400, description = "Unknown status or negative ping"),
        (status = 401, description = "Missing or wrong token (also for an unknown id, so ids cannot be probed)")
    )
)]
pub(crate) async fn push(
    State(state): State<AppState>,
    Path(id): Path<String>,
    query_auth: QueryTokenAuth,
    Query(query): Query<PushQuery>,
    headers: HeaderMap,
    method: Method,
) -> Result<Response, AppError> {
    let config = state.config.borrow().clone();
    // Kuma embeds the credential in the path; map it to the internal ID.
    let compatible = config.monitors.iter().find(|monitor| {
        matches!(&monitor.spec, MonitorKind::Push(push) if push.kuma_compatible)
            && monitor
                .push_token
                .as_ref()
                .is_some_and(|token| ct_eq(&id, token.as_ref()))
    });
    if method == Method::GET && compatible.is_none() {
        return Err(AppError::Unauthorized("invalid push token"));
    }
    let is_kuma = compatible.is_some();
    let id = compatible.map_or(id.as_str(), |monitor| monitor.id.as_str());
    // A push id is either a push monitor or a watched peer's listen id (peers
    // heartbeat the same endpoint); the expected token comes from whichever matches.
    let expected_token = if let Some(monitor) = config
        .monitors
        .iter()
        .find(|monitor| monitor.id == id && monitor.kind() == Kind::Push)
    {
        monitor.push_token.as_ref()
    } else if let Some(peer) = config
        .peers
        .iter()
        .find(|peer| peer.is_watched() && peer.listen_id() == id)
    {
        peer.listen_token.as_ref()
    } else {
        // The same 401 as a wrong token: a 404 here would tell anyone which
        // push ids exist (and so which ones to try tokens on). The operator
        // finds a typo'd id in the log.
        tracing::debug!("push for an unknown id or token");
        return Err(AppError::Unauthorized("invalid push token"));
    };

    // A configured token is required; without one, the id alone authorizes. Prefer
    // the `X-Push-Token` header (kept out of access logs) over the `?token=` query.
    if let Some(expected) = expected_token.filter(|_| !is_kuma) {
        let header = push_token(&headers);
        let provided = header.or(query.token.as_deref());
        if !provided.is_some_and(|token| ct_eq(token, expected.as_ref())) {
            return Err(AppError::Unauthorized("invalid push token"));
        }
        if header.is_none() {
            query_auth.mark();
        }
    }

    // A typo'd status must fail loudly: recording `status=dwon` as "up" would
    // report the opposite of what the job meant.
    let status = match query.status.as_deref() {
        None | Some("") => CheckStatus::Up,
        Some(word) => CheckStatus::parse(word)
            .ok_or(AppError::BadRequest("status must be up, down or degraded"))?,
    };
    let ping = parse_ping(query.ping.as_deref())?;
    // Bound the stored message so a buggy or hostile pusher can't bloat the DB.
    let msg = query.msg.as_deref().map(|msg| {
        hora_core::fmt::printable(msg)
            .chars()
            .take(MAX_PUSH_MSG_CHARS)
            .collect::<String>()
    });
    db::insert_push(&state.store, id, status, ping, msg.as_deref()).await?;
    Ok(if is_kuma {
        Json(serde_json::json!({"ok": true})).into_response()
    } else {
        "ok".into_response()
    })
}

/// Kuma allows fractional milliseconds and an empty ping query.
fn parse_ping(value: Option<&str>) -> Result<Option<i64>, AppError> {
    let Some(value) = value.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let value: f64 = value
        .parse()
        .map_err(|_| AppError::BadRequest("ping must be a non-negative finite number"))?;
    if !value.is_finite() || value < 0.0 {
        return Err(AppError::BadRequest(
            "ping must be a non-negative finite number",
        ));
    }
    format!("{value:.0}")
        .parse()
        .map(Some)
        .map_err(|_| AppError::BadRequest("ping is too large"))
}

/// The JSON body of `POST /api/monitors/{id}/alert`. Every field is optional at
/// the deserialization layer so a missing one yields a clean 400 from the
/// handler rather than a 422 from the extractor.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub(crate) struct AlertRequest {
    /// `info` (default) | `warning` | `error` | `critical`.
    #[serde(default)]
    severity: Option<String>,
    /// Short headline (required, bounded).
    #[serde(default)]
    title: Option<String>,
    /// Free-form detail (optional, bounded).
    #[serde(default)]
    message: Option<String>,
    /// Coalescing key for the server-side anti-flood window (optional).
    #[serde(default)]
    dedup_key: Option<String>,
    /// Arbitrary key/value pairs folded into the message (optional).
    #[serde(default)]
    tags: std::collections::HashMap<String, String>,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct AlertResponse {
    /// `dispatched` (sent to the channels and recorded) or `coalesced` (a
    /// `dedup_key` repeat dropped inside the anti-flood window).
    status: &'static str,
    /// The accepted severity.
    severity: &'static str,
    /// The recorded alert's id - only when `dispatched`.
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<i64>,
    /// The dedup key - echoed only when `coalesced`.
    #[serde(skip_serializing_if = "Option::is_none")]
    dedup_key: Option<String>,
    /// Repeats coalesced in this window so far - only when `coalesced`.
    #[serde(skip_serializing_if = "Option::is_none")]
    suppressed: Option<u64>,
    /// Seconds until the window reopens - only when `coalesced`.
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after_secs: Option<i64>,
}

#[utoipa::path(
    post,
    path = "/api/monitors/{id}/alert",
    params(
        ("id" = String, Path, description = "Monitor id the alert is attached to"),
        ("token" = Option<String>, Query, deprecated, description = "server.auth_token; deprecated (answered with a Deprecation header): use Authorization: Bearer or X-Push-Token")
    ),
    request_body = AlertRequest,
    security(("push_token" = []), ("bearer" = [])),
    responses(
        (status = 202, description = "Alert dispatched to the monitor's channels (or coalesced)", body = AlertResponse),
        (status = 400, description = "Empty title or unknown severity"),
        (status = 401, description = "Missing/wrong X-Push-Token and no matching server.auth_token (also for an unknown monitor)"),
        (status = 404, description = "Unknown monitor (operator token only)")
    )
)]
pub(crate) async fn post_alert(
    State(state): State<AppState>,
    Path(id): Path<String>,
    viewer: Viewer,
    headers: HeaderMap,
    Json(request): Json<AlertRequest>,
) -> Result<(StatusCode, Json<AlertResponse>), AppError> {
    let config = &viewer.config;
    let monitor = config.find_monitor(&id);

    // Authenticate with the monitor's own push_token (preferred, via the
    // X-Push-Token header kept out of access logs) or the global viewer token.
    // Dispatching to channels can flood, so - unlike a read-only view - the
    // endpoint stays closed unless a credential is configured and matches.
    // Without the operator token an unknown id answers the same 401 as a
    // known one, so the endpoint cannot be used to probe private monitor ids.
    let item_token_ok = monitor
        .and_then(|monitor| monitor.push_token.as_ref())
        .is_some_and(|expected| {
            push_token(&headers).is_some_and(|token| ct_eq(token, expected.as_ref()))
        });
    if !item_token_ok && !viewer.is_operator() {
        return Err(AppError::Unauthorized(
            "alerting requires the monitor's push_token (X-Push-Token) or server.auth_token",
        ));
    }
    if !item_token_ok {
        viewer.note_operator_write();
    }
    let monitor = monitor.ok_or(AppError::NotFound("unknown monitor"))?;

    // Validate the body.
    let severity = match request.severity.as_deref() {
        None | Some("") => AlertSeverity::Info,
        Some(value) => AlertSeverity::parse(value).ok_or(AppError::BadRequest(
            "severity must be info, warning, error or critical",
        ))?,
    };
    let title = hora_core::bounded(
        request.title.as_deref().unwrap_or(""),
        MAX_ALERT_TITLE_CHARS,
    );
    if title.is_empty() {
        return Err(AppError::BadRequest("title must not be empty"));
    }
    let dedup_key: Option<String> = request
        .dedup_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(|key| key.chars().take(MAX_ALERT_DEDUP_CHARS).collect());
    // The producer's text, with any tags folded in ("just enrich the message"),
    // bounded so a buggy producer can't bloat the row.
    let message = render_alert_message(request.message.as_deref().unwrap_or(""), &request.tags);

    // Anti-flood: a dedup_key that recurs inside the window is coalesced here,
    // so the rate-limiting lives in Hora and every producer benefits.
    let window = i64::try_from(config.alerts.push_alert_window_secs).unwrap_or(i64::MAX);
    let now = Utc::now().timestamp();
    if let flood::Admission::Coalesce {
        suppressed,
        retry_after,
    } = state.flood.admit(&id, dedup_key.as_deref(), now, window)
    {
        tracing::info!(monitor = %id, suppressed, "pushed alert coalesced");
        return Ok((
            StatusCode::ACCEPTED,
            Json(AlertResponse {
                status: "coalesced",
                severity: severity.as_str(),
                id: None,
                dedup_key,
                suppressed: Some(suppressed),
                retry_after_secs: Some(retry_after),
            }),
        ));
    }

    // Record the timeline line first (so the 202 reflects a durable row), then
    // fan the alert out to the monitor's channels. The dispatch is spawned, so a
    // slow channel never holds up the producer.
    let alert_id = db::insert_pushed_alert(
        &state.store,
        &id,
        severity.as_str(),
        &title,
        &message,
        dedup_key.as_deref(),
    )
    .await?;
    spawn_alert_dispatch(&state, monitor, severity, title.clone(), message);

    tracing::info!(monitor = %id, severity = severity.as_str(), %title, "pushed alert dispatched");
    Ok((
        StatusCode::ACCEPTED,
        Json(AlertResponse {
            status: "dispatched",
            severity: severity.as_str(),
            id: Some(alert_id),
            dedup_key,
            suppressed: None,
            retry_after_secs: None,
        }),
    ))
}

/// Fan a pushed alert out to a monitor's channels in the background, so a slow
/// channel never blocks the 202. The owned strings outlive the request.
fn spawn_alert_dispatch(
    state: &AppState,
    monitor: &Monitor,
    severity: AlertSeverity,
    title: String,
    message: String,
) {
    let notifier = state.notifier.clone();
    let name = monitor.name.clone();
    let notify = monitor.notify.clone();
    tokio::spawn(async move {
        notifier
            .load_full()
            .dispatch(
                Event::Alert {
                    monitor: &name,
                    severity,
                    title: &title,
                    message: &message,
                },
                notify.as_deref(),
            )
            .await;
    });
}

/// Build the stored/sent alert message: the producer's text, then each tag as a
/// `key=value` line (sorted, so the same alert always reads identically). Every
/// part is trimmed and bounded.
fn render_alert_message(message: &str, tags: &std::collections::HashMap<String, String>) -> String {
    let mut out = hora_core::bounded(message, MAX_PUSH_MSG_CHARS);
    let mut pairs: Vec<(&String, &String)> = tags.iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    for (key, value) in pairs.into_iter().take(MAX_ALERT_TAGS) {
        let key = hora_core::bounded(key, MAX_ALERT_TAG_CHARS);
        if key.is_empty() {
            continue;
        }
        let value = hora_core::bounded(value, MAX_ALERT_TAG_CHARS);
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&key);
        out.push('=');
        out.push_str(&value);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;

    #[test]
    fn alert_message_folds_tags_in_sorted_order() {
        let tags = HashMap::from([
            ("task_id".to_owned(), "t1".to_owned()),
            ("patch_id".to_owned(), "9f3c".to_owned()),
        ]);
        // Producer text first, then each tag as `key=value`, keys sorted so the
        // same alert always reads identically.
        assert_eq!(
            render_alert_message("3/12 ops failed", &tags),
            "3/12 ops failed\npatch_id=9f3c\ntask_id=t1"
        );
    }

    #[test]
    fn alert_message_handles_empty_text_and_tags() {
        assert_eq!(render_alert_message("  boom  ", &HashMap::new()), "boom");
        // No producer text: the message is just the tag lines.
        let tags = HashMap::from([("k".to_owned(), "v".to_owned())]);
        assert_eq!(render_alert_message("", &tags), "k=v");
        // Nothing at all yields an empty message.
        assert_eq!(render_alert_message("", &HashMap::new()), "");
    }
}
