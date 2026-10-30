//! The status summary: turning stored checks into the page/API view model.
//!
//! [`build_summary`] gathers everything in batched queries, for the operator;
//! [`derive`] cuts every other audience's view from that in memory. The
//! view-model types are in `view`, a monitor card's assembly in `monitor`,
//! and the status/label helpers shared with the other views in `status`.
//! The refresher that calls them, off the request path, is `crate::snapshot`.

mod monitor;
mod status;
#[cfg(test)]
mod tests;
mod view;

pub(crate) use view::{
    ChannelView, DayCell, GroupView, IncidentView, MaintenanceView, MonitorView, PeerView,
    RecentIncident, StatusTemplate, StorageView, Summary, VantageView,
};

use monitor::{MonitorData, build_groups, build_monitor_view};
pub(crate) use status::DAY_OUTAGE_BELOW_PERMILLE;
use status::{iso, overall_label, worse};

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use hora_core::db::Store;

use hora_core::SECONDS_PER_DAY;
use hora_core::config::{Config, Monitor};
use hora_core::db::{self, Latest};
use hora_core::notifications::ChannelHealthEntry;
use hora_core::status::MonitorState;

use crate::SPARK_BUCKETS;
use crate::visibility::{Audience, Visibility};

/// Shared, read-only inputs for building each monitor's view.
pub(crate) struct SummaryCtx {
    now: DateTime<Utc>,
    timestamp: i64,
    threshold: i64,
    cert_threshold: i64,
    history_days: u16,
}

/// What the background refresher keeps from one build to the next: the
/// incrementally maintained sparkline and daily-bar aggregates (see
/// `hora_core::db::window`).
#[derive(Default)]
pub(crate) struct BuildState {
    window: db::WindowCache,
    sparklines: db::SparklineCache,
    daily: db::DailyCache,
    marks: db::IncidentMarksCache,
}

/// One build: the operator's summary, plus every configured monitor's status
/// (the topology walk of another audience's view needs them all) and the
/// newest incidents unfiltered (each audience lists the ones it may see).
pub(crate) struct Built {
    pub(crate) summary: Arc<Summary>,
    pub(crate) statuses: HashMap<String, MonitorState>,
    pub(crate) incidents: Vec<db::Incident>,
}

/// How many of the newest incidents a build keeps, for every audience to
/// pick its [`RECENT_SHOWN`] from: a public page whose newest incidents are
/// all private ones may list fewer, never another audience's.
const RECENT_SCAN: i64 = 50;
/// Incidents the status page lists under "Recent incidents".
const RECENT_SHOWN: usize = 3;

/// Build the operator's page/API view model - every monitor, full detail,
/// event markers and channel health. Every other audience's view is derived
/// from it in memory ([`derive`]), never from another pass over the database.
pub(crate) async fn build_summary(
    store: &Store,
    config: &Config,
    state: &mut BuildState,
    channel_health: &[ChannelHealthEntry],
    vantage: &HashMap<String, Vec<hora_core::mesh::vantage::PeerVantage>>,
) -> Built {
    let now = Utc::now();
    let timestamp = now.timestamp();
    // The daily fetch also feeds the error-budget arithmetic, so it must cover
    // the largest configured SLO window, not just the displayed history.
    let slo_days = config
        .monitors
        .iter()
        .filter(|monitor| monitor.slo_uptime.is_some())
        .map(Monitor::slo_window_days)
        .max()
        .unwrap_or(0);
    let fetch_days = config.page.history_days.max(slo_days);
    let since_24h = timestamp - SECONDS_PER_DAY;
    let since_history = timestamp - i64::from(fetch_days) * SECONDS_PER_DAY;
    let ctx = SummaryCtx {
        now,
        timestamp,
        threshold: i64::from(config.alerts.fail_threshold.max(1)),
        cert_threshold: i64::from(config.alerts.cert_expiry_days),
        history_days: config.page.history_days,
    };
    let operator = Audience::Operator;
    let visibility = Visibility::new(config, &operator);
    let monitors: Vec<&Monitor> = config.monitors.iter().collect();

    // The batches are independent, so they run concurrently: under WAL every
    // read gets its own pool connection and the build costs the slowest
    // query, not the sum. A failed query degrades to empty data ("no data"
    // cards) rather than blacking out the page. The 24h figures, sparklines
    // and bars come from the hourly roll-ups and caches kept across builds
    // (`state`): a build reads what changed since the last one, never a day
    // of raw checks (the first build after start fills the caches).
    let bucket_secs = (SECONDS_PER_DAY / SPARK_BUCKETS).max(1);
    let BuildState {
        window,
        sparklines,
        daily,
        marks,
    } = state;
    let (window, daily, sparklines, certs, recent, events, marks, logged) = tokio::join!(
        window.refresh(store, since_24h, timestamp),
        daily.refresh(store, since_history, timestamp),
        sparklines.refresh(store, since_24h, bucket_secs, timestamp),
        db::cert_all(store),
        recent_checks_map(store, &monitors, ctx.threshold.max(1)),
        db::events_since(store, since_24h),
        marks.refresh(store),
        db::recent_incidents(store, RECENT_SCAN),
    );
    let window = or_empty(window, "24h window");
    let daily = or_empty(daily, "daily");
    let sparklines = or_empty(sparklines, "latency sparklines");
    let certs = or_empty(certs, "certificates");
    let events = or_empty(events, "events");
    let marks = or_empty(marks, "incident marks");
    let logged = or_empty(logged, "recent incidents");
    let (availability, percentiles) = split_window(&window);
    let statuses: HashMap<String, MonitorState> = recent
        .iter()
        .map(|(id, checks)| {
            (
                id.clone(),
                db::derive_status(checks, i64::from(config.fail_threshold(id))),
            )
        })
        .collect();

    let data = MonitorData {
        recent: &recent,
        statuses: &statuses,
        availability: &availability,
        daily: &daily,
        percentiles: &percentiles,
        sparklines: &sparklines,
        certs: &certs,
        marks: &marks,
        maintenance: &config.maintenance,
        vantage,
    };

    let monitors = cards(&monitors, &ctx, &data, config, &visibility).await;

    let overall = monitors
        .iter()
        .fold(MonitorState::Up, |worst, m| worse(worst, m.status));

    let groups = build_groups(&monitors, &config.monitors);

    // Watched peers form their own section; their state does not roll into the
    // overall badge (it tracks the monitored services, not the surveillance mesh).
    let peers = build_peers(store, config, ctx.threshold).await;

    // Banner order: ad-hoc announcements (the fresh operational news) first,
    // then the config-declared ones. A read failure costs the ad-hoc banners
    // only, never the page.
    let mut incidents = announcement_banners(store, timestamp).await;
    incidents.extend(build_incident_banners(config));

    // Active maintenance windows shown as a top banner (so a long reason never
    // changes a card's height and disturbs the grid).
    let maintenances = build_maintenances(config, now, None);

    let summary = Summary {
        title: config.page.title.clone(),
        overall,
        overall_label: overall_label(overall),
        generated_at: now.to_rfc3339(),
        updated_utc: now.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        incidents,
        maintenances,
        monitors,
        groups,
        peers,
        channels: channel_views(channel_health),
        storage: store.size().await.ok().map(|size| StorageView {
            db_bytes: size.db_bytes,
            reclaimable_bytes: size.free_bytes,
        }),
        events: events.into(),
        recent: Vec::new(),
        quiet_days: None,
    };
    let summary = with_incidents(summary, &logged, config, &visibility, now);
    Built {
        summary: Arc::new(summary),
        statuses,
        incidents: logged,
    }
}

/// Cards built between two yields to the runtime in [`cards`].
const CARDS_PER_YIELD: usize = 64;

/// Every monitor's card, with the maintenance window it is in. Thousands of
/// cards are a second of CPU: the build yields every [`CARDS_PER_YIELD`] so
/// that, on a one-CPU box (a one-worker runtime), the scheduler's probes and
/// inserts are not held behind it.
async fn cards(
    monitors: &[&Monitor],
    ctx: &SummaryCtx,
    data: &MonitorData<'_>,
    config: &Config,
    visibility: &Visibility<'_>,
) -> Vec<Arc<MonitorView>> {
    let mut cards = Vec::with_capacity(monitors.len());
    for chunk in monitors.chunks(CARDS_PER_YIELD) {
        for monitor in chunk {
            let mut view = build_monitor_view(monitor, ctx, data, &config.monitors, visibility);
            view.maintenance = config
                .active_maintenance(&monitor.id, ctx.now)
                .map(|window| window.title.clone());
            cards.push(Arc::new(view));
        }
        tokio::task::yield_now().await;
    }
    cards
}

/// Fill a summary's "Recent incidents" (the newest ones this audience may
/// see, sanitized for it, among its monitors) and its quiet days.
fn with_incidents(
    mut summary: Summary,
    incidents: &[db::Incident],
    config: &Config,
    visibility: &Visibility<'_>,
    now: DateTime<Utc>,
) -> Summary {
    let since = now.timestamp() - i64::from(config.page.history_days) * SECONDS_PER_DAY;
    summary.recent = incidents
        .iter()
        .filter(|incident| incident.ended_at.is_none_or(|ended| ended >= since))
        .filter(|incident| {
            visibility.can_see(&incident.monitor_id)
                && summary.monitors.iter().any(|m| m.id == incident.monitor_id)
        })
        .take(RECENT_SHOWN)
        .map(|incident| {
            let mut incident = incident.clone();
            visibility.sanitize_incident(&mut incident);
            recent_view(&incident, config)
        })
        .collect();
    summary.quiet_days = quiet_days(&summary.monitors, now.timestamp());
    summary
}

/// One incident as the status page lists it.
fn recent_view(incident: &db::Incident, config: &Config) -> RecentIncident {
    let name = config.monitor_name(&incident.monitor_id);
    let started = DateTime::from_timestamp(incident.started_at, 0);
    let title = match (incident.ended_at, incident.duration_s) {
        (Some(_), Some(secs)) => format!(
            "{name} was down for {}",
            crate::layout::minutes((secs + 59) / 60)
        ),
        (Some(_), None) => format!("{name} was down"),
        (None, _) => format!("{name} is down"),
    };
    let note = incident
        .note
        .clone()
        .or_else(|| {
            incident
                .cause
                .as_ref()
                .map(|cause| format!("Caused by {cause}."))
        })
        .or_else(|| {
            incident
                .error
                .as_ref()
                .map(|error| format!("Last answer: {error}."))
        });
    RecentIncident {
        id: incident.id,
        monitor_id: incident.monitor_id.clone(),
        date: started
            .map(|at| at.format("%Y-%m-%d").to_string())
            .unwrap_or_default(),
        day: started
            .map(|at| at.format("%-d %b").to_string())
            .unwrap_or_default(),
        title,
        note,
        resolved: incident.ended_at.is_some(),
    }
}

/// Whole days without an incident among `monitors`: since the last one
/// ended, or, when none ever did, since the oldest day with data. `None`
/// while one is open, or without a single day of data.
pub(crate) fn quiet_days(monitors: &[Arc<MonitorView>], now: i64) -> Option<i64> {
    if monitors.iter().any(|m| m.marks.open_since.is_some()) {
        return None;
    }
    let days = match monitors.iter().filter_map(|m| m.marks.last_end).max() {
        Some(last_end) => (now - last_end).max(0) / SECONDS_PER_DAY,
        None => monitors
            .iter()
            .filter_map(|m| {
                let oldest = m.bar.iter().position(|cell| cell.state != "empty")?;
                i64::try_from(m.bar.len() - oldest).ok()
            })
            .max()?,
    };
    (days > 0).then_some(days)
}

/// The 24h figures as the cards read them: `(available, total)` and the
/// percentiles, per monitor.
fn split_window(
    window: &HashMap<String, db::WindowStats>,
) -> (HashMap<String, (i64, i64)>, HashMap<String, Percentiles>) {
    let availability = window
        .iter()
        .map(|(id, stats)| (id.clone(), (stats.available, stats.total)))
        .collect();
    let percentiles = window
        .iter()
        .filter_map(|(id, stats)| {
            let (p50, p95, p99) = stats.latency.p50_p95_p99()?;
            Some((id.clone(), Percentiles { p50, p95, p99 }))
        })
        .collect();
    (availability, percentiles)
}

/// Derive another audience's view from the operator's: the monitors it may
/// not see are left out entirely - cards, groups and daily bars alike;
/// failure reasons collapse to safe categories (see `probe::public_reason`)
/// without detail; topology annotations never name a monitor it may not see;
/// event markers and channel health are operator streams, so they go; the
/// recent incidents are the ones it may see, sanitized for it. A card that
/// needs none of that is shared with the operator's view as is.
/// `config` is the live one: a monitor made private by a reload disappears
/// from the next request, before the next build.
pub(crate) fn derive(built: &Built, config: &Config, visibility: &Visibility<'_>) -> Summary {
    let operator = &built.summary;
    let monitors: Vec<Arc<MonitorView>> = operator
        .monitors
        .iter()
        .filter(|view| visibility.can_see(&view.id))
        .filter_map(|view| {
            let monitor = config.monitors.iter().find(|m| m.id == view.id)?;
            Some(derive_view(view, monitor, built, config, visibility))
        })
        .collect();
    let overall = monitors
        .iter()
        .fold(MonitorState::Up, |worst, m| worse(worst, m.status));
    let groups = build_groups(&monitors, &config.monitors);
    let summary = Summary {
        title: operator.title.clone(),
        overall,
        overall_label: overall_label(overall),
        generated_at: operator.generated_at.clone(),
        updated_utc: operator.updated_utc.clone(),
        incidents: operator.incidents.clone(),
        maintenances: operator.maintenances.clone(),
        monitors,
        groups,
        peers: operator.peers.clone(),
        channels: Vec::new(),
        storage: None,
        events: Arc::from([]),
        recent: Vec::new(),
        quiet_days: None,
    };
    let now = DateTime::parse_from_rfc3339(&operator.generated_at)
        .map_or_else(|_| Utc::now(), |at| at.with_timezone(&Utc));
    with_incidents(summary, &built.incidents, config, visibility, now)
}

/// One card for a non-operator audience (see [`derive`]).
fn derive_view(
    view: &Arc<MonitorView>,
    monitor: &Monitor,
    built: &Built,
    config: &Config,
    visibility: &Visibility<'_>,
) -> Arc<MonitorView> {
    // The stored reason carries operator detail (body snippets, DNS
    // answers); an audience without detail gets the safe category instead.
    let last_error = view.last_error.as_deref().map(|reason| {
        if visibility.detailed(&view.id) {
            reason.to_owned()
        } else {
            hora_core::probe::public_reason(view.last_reason, reason).to_owned()
        }
    });
    let (cause, impacted) = if view.status == MonitorState::Down {
        monitor::topology_context(monitor, &built.statuses, &config.monitors, visibility)
    } else {
        (None, Vec::new())
    };
    if last_error == view.last_error && cause == view.cause && impacted == view.impacted {
        return Arc::clone(view);
    }
    let mut derived = MonitorView::clone(view);
    derived.last_error = last_error;
    derived.cause = cause;
    derived.impacted = impacted;
    Arc::new(derived)
}

/// Build the banner view for the maintenance windows active at `now`, resolving
/// each covered monitor id to its display name (or "all monitors" when empty).
/// With `only_ids`, windows that touch none of those monitors are dropped (the
/// per-group page must not announce another client's maintenance).
fn build_maintenances(
    config: &Config,
    now: DateTime<Utc>,
    only_ids: Option<&std::collections::HashSet<&str>>,
) -> Vec<MaintenanceView> {
    config
        .maintenance
        .iter()
        .filter(|window| window.active_span(now).is_some())
        .filter(|window| {
            only_ids.is_none_or(|ids| {
                window.monitors.is_empty()
                    || window.monitors.iter().any(|id| ids.contains(id.as_str()))
            })
        })
        .map(|window| {
            let monitors = if window.monitors.is_empty() {
                "all monitors".to_owned()
            } else {
                window
                    .monitors
                    .iter()
                    .map(|id| {
                        config
                            .monitors
                            .iter()
                            .find(|monitor| &monitor.id == id)
                            .map_or(id.as_str(), |monitor| monitor.name.as_str())
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            MaintenanceView {
                reason: window.title.clone(),
                monitors,
            }
        })
        .collect()
}

/// Build the channel-health view model from the dispatcher's snapshot. Only
/// the operator's view carries it; every derived summary omits it.
fn channel_views(health: &[ChannelHealthEntry]) -> Vec<ChannelView> {
    health
        .iter()
        .map(|entry| ChannelView {
            name: entry.name.clone(),
            failing: entry.health.consecutive_failures > 0,
            consecutive_failures: entry.health.consecutive_failures,
            failing_for_secs: entry
                .health
                .first_failure_at
                .and_then(|since| since.elapsed().ok())
                .map(|d| d.as_secs()),
        })
        .collect()
}

/// Derive a single-group view from a built summary: the monitors of `group`
/// only, the overall badge recomputed from them, the peers section hidden
/// (the surveillance mesh is the operator's business, not a client's), and
/// the maintenance banners restricted to windows touching the group. `None`
/// when the summary holds no monitor of that group - an unknown group, or a
/// fully private one viewed anonymously, answers exactly like a missing page.
pub(crate) fn for_group(summary: &Summary, config: &Config, group: &str) -> Option<Summary> {
    let monitors: Vec<Arc<MonitorView>> = summary
        .monitors
        .iter()
        .filter(|monitor| monitor.group.as_deref() == Some(group))
        .map(Arc::clone)
        .collect();
    if monitors.is_empty() {
        return None;
    }

    let overall = monitors.iter().fold(MonitorState::Up, |worst, monitor| {
        worse(worst, monitor.status)
    });
    let ids: std::collections::HashSet<&str> =
        monitors.iter().map(|monitor| monitor.id.as_str()).collect();
    let maintenances = build_maintenances(config, Utc::now(), Some(&ids));

    Some(Summary {
        title: format!("{} · {group}", config.page.title),
        overall,
        overall_label: overall_label(overall),
        generated_at: summary.generated_at.clone(),
        updated_utc: summary.updated_utc.clone(),
        // The operator's announcements are page-wide communication; keep them
        // (the already-built list includes the ad-hoc `hora announce` ones).
        incidents: summary.incidents.clone(),
        maintenances,
        groups: vec![GroupView {
            name: group.to_owned(),
            ids: monitors.iter().map(|view| view.id.clone()).collect(),
            monitors: monitors.clone(),
        }],
        peers: Vec::new(),
        channels: Vec::new(),
        storage: None,
        events: Arc::clone(&summary.events),
        recent: summary
            .recent
            .iter()
            .filter(|incident| ids.contains(incident.monitor_id.as_str()))
            .cloned()
            .collect(),
        quiet_days: quiet_days(&monitors, Utc::now().timestamp()),
        monitors,
    })
}

/// The ad-hoc announcements (`hora announce` / `POST /api/announce`),
/// rendered like the config-declared banners. Newest first.
async fn announcement_banners(store: &Store, now: i64) -> Vec<IncidentView> {
    or_empty(db::active_announcements(store, now).await, "announcements")
        .into_iter()
        .map(|announcement| IncidentView {
            title: announcement.title,
            body: announcement.body,
            severity: announcement.severity.as_str(),
            at: chrono::DateTime::from_timestamp(announcement.created_at, 0)
                .map(|at| at.format("%Y-%m-%d %H:%M UTC").to_string()),
        })
        .collect()
}

/// The static severity label for a stored announcement (validated at insert;
/// anything unexpected degrades to `info`).
/// The configured announcements, rendered for the status page banner.
fn build_incident_banners(config: &Config) -> Vec<IncidentView> {
    config
        .incidents
        .iter()
        .map(|incident| IncidentView {
            title: incident.title.clone(),
            body: incident.body.clone(),
            severity: incident.severity.as_str(),
            at: incident
                .at
                .map(|at| at.format("%Y-%m-%d %H:%M UTC").to_string()),
        })
        .collect()
}

/// Build the view for each watched peer (its current status and last-seen time),
/// rendered in a section of its own apart from the monitors.
async fn build_peers(store: &Store, config: &Config, threshold: i64) -> Vec<PeerView> {
    let mut peers = Vec::new();
    for peer in config.peers.iter().filter(|peer| peer.is_watched()) {
        let recent = or_empty(
            db::recent_checks(store, peer.listen_id(), threshold.max(1)).await,
            "peer recent checks",
        );
        peers.push(PeerView {
            status: db::derive_status(&recent, threshold),
            last_seen: recent.first().and_then(|latest| iso(latest.time)),
            id: peer.id.clone(),
            name: peer.name.clone(),
            pings: peer.ping_url.is_some(),
        });
    }
    peers
}

/// Unwrap a batch query, logging and using empty data on error so one failed
/// query degrades to "no data" cards instead of failing the whole page.
pub(crate) fn or_empty<T: Default>(result: hora_core::db::Result<T>, what: &str) -> T {
    result.unwrap_or_else(|err| {
        tracing::error!("summary: {what} query failed: {err:#}");
        T::default()
    })
}

/// Fetch each monitor's recent checks. Deliberately per-monitor: the query is an
/// indexed `ORDER BY time DESC LIMIT N` (tiny), and unlike a single windowed query
/// it is correct for any interval - a monitor checked less than once a day (e.g. a
/// weekly push heartbeat) would be dropped by a 24h batch window and shown as
/// "unknown". A few run at a time: one by one, their round trips added up to a
/// fifth of a second for 700 monitors.
pub(crate) async fn recent_checks_map(
    store: &Store,
    monitors: &[&Monitor],
    limit: i64,
) -> HashMap<String, Vec<Latest>> {
    use futures_util::StreamExt as _;

    // Owned ids and pool handles: borrowed ones trip the compiler's
    // higher-ranked lifetime inference once this future is spawned.
    let ids: Vec<(String, i64)> = monitors
        .iter()
        .map(|monitor| {
            (
                monitor.id.clone(),
                monitor.fail_threshold.map_or(limit, i64::from),
            )
        })
        .collect();
    futures_util::stream::iter(ids)
        .map(|(id, limit)| {
            let store = store.clone();
            async move {
                let checks = or_empty(db::recent_checks(&store, &id, limit).await, "recent checks");
                (id, checks)
            }
        })
        .buffer_unordered(RECENT_CHECKS_CONCURRENCY)
        .collect()
        .await
}

/// How many [`recent_checks_map`] reads run at once: a share of the pool,
/// leaving connections to the build's other queries.
const RECENT_CHECKS_CONCURRENCY: usize = 4;

/// 24h latency percentiles for a monitor (nearest-rank over the merged
/// hourly histograms, see `hora_core::histogram`).
#[derive(Clone, Copy)]
pub(crate) struct Percentiles {
    p50: i64,
    p95: i64,
    p99: i64,
}
