//! One monitor's card: its view built from the batched data, its vantage and
//! topology context, the error-budget line, and the group sections.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::TimeDelta;

use hora_core::SECONDS_PER_DAY;
use hora_core::config::Monitor;
use hora_core::db::{self, DayRow, Latest, Point};
use hora_core::status::MonitorState;
use hora_core::{slo, topology};

use super::status::{build_bar, cert_label, cert_state_for, iso, slo_state};
use super::view::{GroupView, MonitorView, VantageView};
use super::{Percentiles, SummaryCtx};

use crate::visibility::Visibility;

/// The pre-fetched batch maps a monitor's view is assembled from, keyed by
/// monitor id. Grouped so [`build_monitor_view`] stays a small, pure function.
pub(crate) struct MonitorData<'a> {
    pub(super) recent: &'a HashMap<String, Vec<Latest>>,
    /// Every monitor's current status, for the topology walk.
    pub(super) statuses: &'a HashMap<String, MonitorState>,
    pub(super) availability: &'a HashMap<String, (i64, i64)>,
    pub(super) daily: &'a HashMap<String, Vec<DayRow>>,
    pub(super) percentiles: &'a HashMap<String, Percentiles>,
    pub(super) sparklines: &'a HashMap<String, Vec<Point>>,
    pub(super) certs: &'a HashMap<String, i64>,
    /// Each monitor's open incident and last finished one.
    pub(super) marks: &'a HashMap<String, db::IncidentMarks>,
    /// Cached calendar days touched by each configured maintenance window.
    pub(super) maintenance: &'a super::maintenance::MaintenanceCalendar,
    /// The peers' view of shared targets, from the daemon's vantage poller.
    pub(super) vantage: &'a HashMap<String, Vec<hora_core::mesh::vantage::PeerVantage>>,
}

/// Availability over the bar's days: up or slow checks over all of them.
fn long_uptime(daily: &[DayRow], bar: &[super::view::DayCell]) -> Option<i64> {
    let first = bar.first()?.date.as_str();
    let (available, total) = daily.iter().filter(|row| row.day.as_str() >= first).fold(
        (0_i64, 0_i64),
        |(available, total), row| {
            (
                available + row.up + row.degraded,
                total + row.up + row.down + row.degraded,
            )
        },
    );
    (total > 0).then(|| available.saturating_mul(1000) / total)
}

/// Build a monitor's view from the pre-fetched batch maps. Pure: a monitor with
/// no data simply renders an empty ("no data yet") card.
pub(crate) fn build_monitor_view(
    monitor: &Monitor,
    ctx: &SummaryCtx,
    data: &MonitorData,
    all_monitors: &[Monitor],
    visibility: &Visibility<'_>,
) -> MonitorView {
    let recent = data
        .recent
        .get(&monitor.id)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let status = db::derive_status(
        recent,
        monitor.fail_threshold.map_or(ctx.threshold, i64::from),
    );

    let uptime_permille = data
        .availability
        .get(&monitor.id)
        .and_then(|&(available, total)| {
            (total > 0).then(|| available.saturating_mul(1000) / total)
        });

    let daily = data
        .daily
        .get(&monitor.id)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut bar = build_bar(daily, ctx.now, ctx.history_days);
    data.maintenance.mark(&mut bar, &monitor.id);
    let uptime_long_permille = long_uptime(daily, &bar);

    // Kept as points, not drawn: only the monitor's own page shows the
    // chart, so it is drawn there, for the one monitor asked for.
    let spark: Arc<[Point]> = data.sparklines.get(&monitor.id).map_or_else(
        || Arc::from(Vec::new()),
        |points| Arc::from(points.as_slice()),
    );
    let pct = data.percentiles.get(&monitor.id).copied();
    let slo_state = slo_state(monitor.slo_latency_ms, pct.map(|p| p.p95));

    let cert_days = data
        .certs
        .get(&monitor.id)
        .map(|&not_after| (not_after - ctx.timestamp) / SECONDS_PER_DAY);

    let latest = recent.first();
    let (cause, impacted) = if status == MonitorState::Down {
        topology_context(monitor, data.statuses, all_monitors, visibility)
    } else {
        (None, Vec::new())
    };
    let budget = budget_view(monitor, ctx, daily);

    MonitorView {
        id: monitor.id.clone(),
        name: monitor.name.clone(),
        status,
        last_latency_ms: latest.and_then(|l| l.latency_ms),
        // The stored reason carries operator detail (body snippets, DNS
        // answers); an audience without detail gets the safe category instead.
        last_error: latest.and_then(|l| l.error.as_deref()).map(|reason| {
            if visibility.detailed(&monitor.id) {
                reason.to_owned()
            } else {
                hora_core::probe::public_reason(latest.and_then(|l| l.reason), reason).to_owned()
            }
        }),
        last_reason: latest.and_then(|l| l.reason),
        last_checked: latest.and_then(|l| iso(l.time)),
        uptime_permille,
        uptime_label: uptime_permille.map(hora_core::fmt::permille),
        p50_ms: pct.map(|p| p.p50),
        p95_ms: pct.map(|p| p.p95),
        p99_ms: pct.map(|p| p.p99),
        slo_latency_ms: monitor.slo_latency_ms,
        slo_state,
        slo_uptime_pct: monitor.slo_uptime.map(|bp| f64::from(bp) / 100.0),
        budget_minutes_total: budget.as_ref().map(|b| b.total),
        budget_minutes_left: budget.as_ref().map(|b| b.left),
        budget_label: budget.as_ref().map(|b| b.label.clone()),
        budget_title: budget.as_ref().map(|b| b.title.clone()).unwrap_or_default(),
        budget_state: budget.as_ref().map_or("none", |b| b.state),
        maintenance: None,
        slow_over_ms: monitor.degraded_over_ms,
        down_since: (status == MonitorState::Down)
            .then(|| {
                data.marks
                    .get(&monitor.id)
                    .and_then(|marks| marks.open_since)
            })
            .flatten(),
        marks: data.marks.get(&monitor.id).copied().unwrap_or_default(),
        uptime_long_permille,
        cert_days,
        cert_label: cert_days.map(cert_label),
        cert_state: cert_state_for(cert_days, ctx.cert_threshold),
        bar: bar.into(),
        spark,
        group: monitor.group.clone(),
        cause,
        impacted,
        vantages: vantage_views(data.vantage, monitor),
    }
}

/// The per-vantage line of a card: each peer's view of this target, with a
/// pre-formatted label ("Hora B: 220ms", "Hora C: down").
fn vantage_views(
    map: &HashMap<String, Vec<hora_core::mesh::vantage::PeerVantage>>,
    monitor: &Monitor,
) -> Vec<VantageView> {
    hora_core::mesh::vantage::for_monitor(map, monitor)
        .into_iter()
        .map(|view| {
            let label = match (view.status.as_str(), view.p50_ms) {
                ("up" | "degraded", Some(ms)) => format!("{}: {ms}ms", view.peer),
                (status, _) => format!("{}: {status}", view.peer),
            };
            VantageView {
                peer: view.peer,
                status: view.status,
                p50_ms: view.p50_ms,
                label,
            }
        })
        .collect()
}

/// Compute topology annotation for a down monitor: the nearest down upstream
/// name (`cause`) or the list of impacted dependent names. Only monitors the
/// audience may see are named - a private upstream/dependent must not leak its
/// name through a card's annotation. The graph is still walked in full, so a
/// nameable down ancestor further up is still named.
pub(super) fn topology_context(
    monitor: &Monitor,
    statuses: &HashMap<String, MonitorState>,
    all_monitors: &[Monitor],
    visibility: &Visibility<'_>,
) -> (Option<String>, Vec<String>) {
    let nameable = |id: &str| visibility.can_name(id);

    let upstreams = topology::transitive_upstreams(all_monitors, &monitor.id);
    for up_id in &upstreams {
        if statuses.get(*up_id).copied() == Some(MonitorState::Down)
            && nameable(up_id)
            && let Some(name) = topology::monitor_name(all_monitors, up_id)
        {
            return (Some(name.to_owned()), Vec::new());
        }
    }

    let dependents = topology::transitive_dependents(all_monitors, &monitor.id);
    let impacted: Vec<String> = dependents
        .iter()
        .filter(|dep_id| nameable(dep_id))
        .filter_map(|dep_id| topology::monitor_name(all_monitors, dep_id).map(String::from))
        .collect();

    (None, impacted)
}

/// Group monitors by their `group` field. Groups appear in config order;
/// ungrouped monitors (no `group`) appear last under an empty-string key.
pub(super) fn build_groups(
    monitors: &[Arc<MonitorView>],
    config_monitors: &[Monitor],
) -> Vec<GroupView> {
    use std::collections::BTreeMap;

    let mut group_order: Vec<String> = Vec::new();
    let mut seen_groups: std::collections::HashSet<String> = std::collections::HashSet::new();
    for monitor in config_monitors {
        if let Some(group) = &monitor.group
            && seen_groups.insert(group.clone())
        {
            group_order.push(group.clone());
        }
    }
    // Ungrouped monitors (empty key) always render last, after every named group
    // (otherwise a headerless card could appear above the first group's header).
    group_order.push(String::new());

    let mut grouped: BTreeMap<String, Vec<Arc<MonitorView>>> = BTreeMap::new();
    for monitor in monitors {
        let key = monitor.group.clone().unwrap_or_default();
        grouped.entry(key).or_default().push(Arc::clone(monitor));
    }

    let mut groups = Vec::new();
    for key in &group_order {
        if let Some(mons) = grouped.remove(key) {
            groups.push(GroupView {
                name: key.clone(),
                ids: mons.iter().map(|view| view.id.clone()).collect(),
                monitors: mons,
            });
        }
    }
    groups
}

/// Error-budget figures for a monitor with an availability SLO.
struct BudgetView {
    total: i64,
    left: i64,
    label: String,
    title: String,
    state: &'static str,
}

/// Compute the error budget over the monitor's SLO window from the merged
/// daily rows (which extend beyond raw retention thanks to the aggregates).
/// Coverage counts only days with data, so a young monitor shows a mostly
/// intact budget rather than a fictional one.
fn budget_view(monitor: &Monitor, ctx: &SummaryCtx, daily: &[DayRow]) -> Option<BudgetView> {
    let slo_bp = monitor.slo_uptime?;
    let window = monitor.slo_window_days();
    let cutoff = (ctx.now - TimeDelta::days(i64::from(window)))
        .format("%Y-%m-%d")
        .to_string();
    let mut available = 0_i64;
    let mut total = 0_i64;
    let mut days = 0_i64;
    // ISO dates compare lexicographically, so a string cutoff suffices.
    for row in daily.iter().filter(|row| row.day >= cutoff) {
        available += row.up + row.degraded;
        total += row.up + row.down + row.degraded;
        days += 1;
    }
    let covered_minutes = days.min(i64::from(window)) * 1440;
    let budget_total = slo::budget_minutes(window, slo_bp);
    let left = (budget_total - slo::consumed_minutes(available, total, covered_minutes)).max(0);
    let state = if left == 0 {
        "breached"
    } else if left.saturating_mul(4) <= budget_total {
        "low"
    } else {
        "met"
    };
    Some(BudgetView {
        total: budget_total,
        left,
        label: format!("budget {}", format_minutes(left)),
        title: format!(
            "error budget: {} of {} left ({}% over {window}d)",
            format_minutes(left),
            format_minutes(budget_total),
            format_slo_pct(slo_bp),
        ),
        state,
    })
}

/// `"43m"`, `"3h07m"`, `"3d2h"` - budget durations at a human size.
pub(super) fn format_minutes(minutes: i64) -> String {
    if minutes >= 2 * 1440 {
        format!("{}d{}h", minutes / 1440, (minutes % 1440) / 60)
    } else if minutes >= 120 {
        format!("{}h{:02}m", minutes / 60, minutes % 60)
    } else {
        format!("{minutes}m")
    }
}

/// Basis points back to a percent string without trailing zeros: 9990 → "99.9".
pub(super) fn format_slo_pct(basis_points: u32) -> String {
    if basis_points.is_multiple_of(100) {
        format!("{}", basis_points / 100)
    } else if basis_points.is_multiple_of(10) {
        format!("{}.{}", basis_points / 100, (basis_points % 100) / 10)
    } else {
        format!("{}.{:02}", basis_points / 100, basis_points % 100)
    }
}
