//! Healthy push watches sleep until their UTC deadline or a committed push.
//! Failures retain the existing evaluation cadence for retries/history/reminders.

#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::time::Instant;

use crate::config::{Monitor, PushSpec};
use crate::db::{PushSubscription, Store};
use crate::heartbeat::{HeartbeatWatch, heartbeat_evaluation_for};
use crate::probe::Outcome;
use crate::status::CheckStatus;

/// A cheap clock/liveness check, with no SQLite access: wall-clock corrections
/// must not leave a monotonic sleep pointing hours beyond the UTC deadline.
pub(crate) const CLOCK_CHECK_SECS: u64 = 60;

enum NextCheck {
    Utc(Option<i64>),
    Retry(Instant),
}

impl NextCheck {
    fn remaining(&self, now: chrono::DateTime<chrono::Utc>, instant: Instant) -> Duration {
        match self {
            Self::Utc(Some(at)) => chrono::DateTime::from_timestamp(*at, 0)
                .and_then(|at| (at - now).to_std().ok())
                .unwrap_or_default(),
            Self::Utc(None) => Duration::from_secs(CLOCK_CHECK_SECS),
            Self::Retry(at) => at.saturating_duration_since(instant),
        }
    }
}

pub(super) struct PushDriver {
    watch: HeartbeatWatch,
    changes: PushSubscription,
    next: NextCheck,
    deadline: Option<i64>,
    last_status: Option<CheckStatus>,
    evaluated_at: Instant,
}

impl PushDriver {
    pub(super) async fn new(store: &Store, monitor: &Monitor, push: &PushSpec) -> Self {
        // Subscribe before any reads: pushes during startup/reload stay pending.
        let changes = store.subscribe_push(&monitor.id);
        Self {
            changes,
            watch: HeartbeatWatch::for_monitor(store, monitor, push).await,
            next: NextCheck::Retry(Instant::now()),
            deadline: None,
            last_status: None,
            evaluated_at: Instant::now(),
        }
    }

    pub(super) async fn wait(&mut self, last_tick: &AtomicU64) {
        loop {
            let remaining = self.next.remaining(chrono::Utc::now(), Instant::now());
            if remaining.is_zero() {
                return;
            }
            tokio::select! {
                () = self.changes.changed() => return,
                () = tokio::time::sleep(remaining.min(Duration::from_secs(CLOCK_CHECK_SECS))) => {
                    last_tick.fetch_max(u64::try_from(chrono::Utc::now().timestamp()).unwrap_or(0), Ordering::Relaxed);
                }
            }
        }
    }

    pub(super) async fn evaluate(&mut self, store: &Store, monitor: &Monitor) -> Option<Outcome> {
        self.changes.mark_seen();
        let now = chrono::Utc::now();
        let evaluation = match heartbeat_evaluation_for(
            store,
            &monitor.id,
            &self.watch.cadence,
            self.watch.expected_since,
            now.timestamp(),
        )
        .await
        {
            Ok(evaluation) => evaluation,
            Err(err) => {
                tracing::error!(monitor = %monitor.id, "failed to read last heartbeat: {err:#}");
                self.next = NextCheck::Retry(
                    Instant::now() + monitor.poll_interval().min(Duration::from_secs(30)),
                );
                return None;
            }
        };
        let status = evaluation.outcome.as_ref().map(|outcome| outcome.status);
        // A burst of identical failures must neither accelerate the failure
        // threshold nor postpone its retry timer. Recovery/status changes wake
        // immediately; another down/degraded observation waits for its cadence.
        if status == self.last_status
            && matches!(status, Some(CheckStatus::Down | CheckStatus::Degraded))
            && !self.next.remaining(now, Instant::now()).is_zero()
        {
            return None;
        }
        self.last_status = status;
        self.deadline = evaluation.deadline;
        self.evaluated_at = Instant::now();
        self.next = NextCheck::Utc(self.deadline);
        evaluation.outcome
    }

    pub(super) fn plan(
        &mut self,
        monitor: &Monitor,
        outcome: &Outcome,
        followup: bool,
        pending: bool,
    ) {
        self.next = if outcome.status != CheckStatus::Up || followup {
            let delay = if pending {
                monitor
                    .retry_interval_secs
                    .map_or_else(|| monitor.poll_interval(), Duration::from_secs)
            } else {
                monitor.poll_interval()
            };
            NextCheck::Retry(self.evaluated_at + delay)
        } else {
            NextCheck::Utc(self.deadline)
        };
    }
}
