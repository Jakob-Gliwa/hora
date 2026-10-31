//! Fixed and recurring maintenance windows, with a timezone-aware cron compiled at load.

use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use serde::Deserialize;

use super::{Compiled, parse_cron};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawMaintenance")]
pub struct Maintenance {
    pub title: String,
    pub monitors: Vec<String>,
    timing: Timing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Timing {
    Fixed {
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    },
    Recurring {
        cron: Box<Compiled<croner::Cron>>,
        timezone: chrono_tz::Tz,
        duration: TimeDelta,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMaintenance {
    #[serde(default)]
    title: String,
    #[serde(default)]
    monitors: Vec<String>,
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
    cron: Option<String>,
    timezone: Option<String>,
    duration_secs: Option<u32>,
}

impl TryFrom<RawMaintenance> for Maintenance {
    type Error = String;
    fn try_from(raw: RawMaintenance) -> Result<Self, Self::Error> {
        let timing =
            match (raw.start, raw.end, raw.cron) {
                (Some(start), Some(end), None)
                    if raw.duration_secs.is_none() && raw.timezone.is_none() =>
                {
                    Timing::Fixed { start, end }
                }
                (None, None, Some(cron)) => {
                    parse_cron(&cron).map_err(|_| "maintenance: invalid five-field cron")?;
                    let timezone = raw
                        .timezone
                        .as_deref()
                        .unwrap_or("UTC")
                        .parse()
                        .map_err(|_| "maintenance: unknown IANA timezone")?;
                    let duration = raw
                        .duration_secs
                        .filter(|s| (1..=604_800).contains(s))
                        .ok_or("maintenance: duration_secs must be 1..604800")?;
                    Timing::Recurring {
                        cron: Box::new(Compiled::parse(&cron)?),
                        timezone,
                        duration: TimeDelta::seconds(i64::from(duration)),
                    }
                }
                _ => return Err(
                    "maintenance: use start < end OR cron with duration_secs and optional timezone"
                        .into(),
                ),
            };
        Ok(Self {
            title: raw.title,
            monitors: raw.monitors,
            timing,
        })
    }
}

impl Maintenance {
    pub(super) fn validate(&self) -> anyhow::Result<()> {
        if let Timing::Fixed { start, end } = &self.timing {
            anyhow::ensure!(start < end, "maintenance window: start must be before end");
        }
        Ok(())
    }

    /// The occurrence active now. Fixed windows preserve their inclusive end;
    /// recurring windows end exactly `duration_secs` after the occurrence.
    #[must_use]
    pub fn active_span(&self, now: DateTime<Utc>) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        match &self.timing {
            Timing::Fixed { start, end } => {
                (now >= *start && now <= *end).then_some((*start, *end))
            }
            Timing::Recurring {
                cron,
                timezone,
                duration,
            } => {
                let start = cron
                    .find_previous_occurrence(&now.with_timezone(timezone), true)
                    .ok()?
                    .with_timezone(&Utc);
                let end = start.checked_add_signed(*duration)?;
                (now < end).then_some((start, end))
            }
        }
    }

    /// Next occurrence strictly after now, for the status-page announcement.
    #[must_use]
    pub fn next_span(&self, now: DateTime<Utc>) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
        match &self.timing {
            Timing::Fixed { start, end } => (*start > now).then_some((*start, *end)),
            Timing::Recurring {
                cron,
                timezone,
                duration,
            } => {
                let start = cron
                    .find_next_occurrence(&now.with_timezone(timezone), false)
                    .ok()?
                    .with_timezone(&Utc);
                Some((start, start.checked_add_signed(*duration)?))
            }
        }
    }

    /// Whether a UTC day in the history bar/report intersects an occurrence.
    #[must_use]
    pub fn touches_utc_day(&self, day: NaiveDate) -> bool {
        let Some(start) = day.and_hms_opt(0, 0, 0).map(|d| d.and_utc()) else {
            return false;
        };
        let Some(end) = start.checked_add_signed(TimeDelta::days(1)) else {
            return false;
        };
        self.active_span(start).is_some()
            || self.next_span(start).is_some_and(|(next, _)| next < end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }
    fn daily() -> Maintenance {
        toml::from_str(
            "cron='0 0 * * *'\ntimezone='Europe/Berlin'\nduration_secs=900\nmonitors=['job']",
        )
        .unwrap()
    }

    #[test]
    fn recurring_maintenance_tracks_local_time_and_exact_end() {
        let window = daily();
        for (before, start, end) in [
            (
                "2026-01-01T22:59:59Z",
                "2026-01-01T23:00:00Z",
                "2026-01-01T23:15:00Z",
            ),
            (
                "2026-07-01T21:59:59Z",
                "2026-07-01T22:00:00Z",
                "2026-07-01T22:15:00Z",
            ),
            (
                "2026-03-29T21:59:59Z",
                "2026-03-29T22:00:00Z",
                "2026-03-29T22:15:00Z",
            ),
            (
                "2026-10-25T22:59:59Z",
                "2026-10-25T23:00:00Z",
                "2026-10-25T23:15:00Z",
            ),
        ] {
            assert!(window.active_span(at(before)).is_none());
            assert_eq!(window.next_span(at(before)), Some((at(start), at(end))));
            assert!(window.active_span(at(start)).is_some());
            assert!(
                window
                    .active_span(at(end) - TimeDelta::seconds(1))
                    .is_some()
            );
            assert!(window.active_span(at(end)).is_none());
        }
    }

    #[test]
    fn recurring_maintenance_scope_and_history() {
        let config = super::super::parse("[page]\n[server]\n[[monitors]]\nid='job'\nname='Job'\nkind='push'\ninterval_secs=60\n[[maintenance]]\ncron='0 13 * * 0'\ntimezone='Europe/Berlin'\nduration_secs=900\nmonitors=['job']").unwrap();
        assert!(config.in_maintenance("job", at("2026-07-05T11:10:00Z")));
        assert!(!config.in_maintenance("other", at("2026-07-05T11:10:00Z")));
        assert!(!config.in_maintenance("job", at("2026-07-06T11:10:00Z")));
        assert!(config.maintenance[0].touches_utc_day(at("2026-07-05T00:00:00Z").date_naive()));
        assert!(!config.maintenance[0].touches_utc_day(at("2026-07-06T00:00:00Z").date_naive()));
    }

    #[test]
    fn recurring_maintenance_rejects_ambiguous_or_invalid_config() {
        for fields in [
            "cron='bad'\nduration_secs=60",
            "cron='0 0 * * *'",
            "cron='0 0 * * *'\nduration_secs=0",
            "cron='0 0 * * *'\nduration_secs=60\ntimezone='invalid'",
            "start='2026-01-01T00:00:00Z'\nend='2026-01-02T00:00:00Z'\ncron='0 0 * * *'\nduration_secs=60",
        ] {
            assert!(toml::from_str::<Maintenance>(fields).is_err());
        }
    }
}
