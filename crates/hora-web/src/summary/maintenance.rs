//! Historical maintenance is a calendar property, shared by all monitor cards.
//! Keep one bounded day mask per window; only a new day or config change needs
//! another cron search. Live maintenance/alert suppression uses the core rules.

use chrono::{NaiveDate, TimeDelta};
use hora_core::config::Maintenance;

use super::view::DayCell;

#[derive(Default)]
pub(super) struct MaintenanceCalendar {
    windows: Vec<Maintenance>,
    today: Option<NaiveDate>,
    days: u16,
    marks: Vec<Vec<bool>>,
    #[cfg(test)]
    evaluations: usize,
}

impl MaintenanceCalendar {
    pub(super) fn refresh(&mut self, windows: &[Maintenance], today: NaiveDate, days: u16) {
        let unchanged = self.windows == windows && self.days == days;
        if unchanged && self.today == Some(today) {
            return;
        }
        let advance = self.today.map(|previous| (today - previous).num_days());
        let shift = if unchanged {
            advance.filter(|&n| n > 0 && n < i64::from(days))
        } else {
            None
        };
        let count = usize::from(days);
        let first_new = if let Some(shift) = shift {
            let shift = usize::try_from(shift).expect("positive day shift within u16");
            for marks in &mut self.marks {
                marks.rotate_left(shift);
            }
            count - shift
        } else {
            self.windows = windows.to_vec();
            self.marks = vec![vec![false; count]; windows.len()];
            0
        };
        for (window, marks) in self.windows.iter().zip(&mut self.marks) {
            for (index, mark) in marks.iter_mut().enumerate().skip(first_new) {
                let offset = i64::try_from(count - index - 1).expect("day offset within u16");
                *mark = window.touches_utc_day(today - TimeDelta::days(offset));
                #[cfg(test)]
                {
                    self.evaluations += 1;
                }
            }
        }
        self.today = Some(today);
        self.days = days;
    }

    /// `bar` uses the same date and history length as `refresh`, oldest first.
    pub(super) fn mark(&self, bar: &mut [DayCell], id: &str) {
        debug_assert_eq!(bar.len(), usize::from(self.days));
        for (window, marks) in self.windows.iter().zip(&self.marks) {
            if window.monitors.is_empty() || window.monitors.iter().any(|m| m == id) {
                for (cell, &mark) in bar.iter_mut().zip(marks) {
                    cell.maint |= mark;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::summary::status::build_bar;

    fn windows() -> Vec<Maintenance> {
        hora_core::config::parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "one"
            name = "One"
            target = "https://example.invalid"
            interval_secs = 60
            [[maintenance]]
            title = "Local night crossing UTC midnight"
            cron = "30 1 * * *"
            timezone = "Europe/Berlin"
            duration_secs = 7200
            monitors = ["one"]
            [[maintenance]]
            title = "Global fixed window ending at midnight"
            start = "2026-03-28T22:00:00Z"
            end = "2026-03-29T00:00:00Z"
            "#,
        )
        .expect("valid config")
        .maintenance
    }

    fn date(raw: &str) -> NaiveDate {
        NaiveDate::parse_from_str(raw, "%Y-%m-%d").unwrap()
    }

    fn assert_marks(cache: &MaintenanceCalendar, windows: &[Maintenance]) {
        let now = cache
            .today
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc();
        for id in ["one", "other"] {
            let mut bar = build_bar(&[], now, cache.days);
            cache.mark(&mut bar, id);
            for cell in bar {
                let expected = windows.iter().any(|window| {
                    (window.monitors.is_empty() || window.monitors.iter().any(|m| m == id))
                        && window.touches_utc_day(date(&cell.date))
                });
                assert_eq!(cell.maint, expected, "{id} {}", cell.date);
            }
        }
    }

    #[test]
    fn reused_across_cards_refreshes_and_equal_config_reloads() {
        let windows = windows();
        let mut cache = MaintenanceCalendar::default();
        cache.refresh(&windows, date("2026-03-30"), 90);
        assert_eq!(cache.evaluations, 180);
        for _ in 0..10 {
            cache.refresh(&windows.clone(), date("2026-03-30"), 90);
            assert_marks(&cache, &windows);
        }
        assert_eq!(cache.evaluations, 180);
        cache.refresh(&windows, date("2026-03-31"), 90);
        assert_eq!(cache.evaluations, 182);
        assert_marks(&cache, &windows);
        cache.refresh(&windows, date("2026-04-02"), 90);
        assert_eq!(cache.evaluations, 186);
        assert_marks(&cache, &windows);
    }

    #[test]
    fn matches_core_across_dst_clock_jumps_and_history_resizes() {
        let windows = windows();
        let mut cache = MaintenanceCalendar::default();
        for (today, days) in [
            ("2026-03-28", 4),
            ("2026-03-29", 4),
            ("2026-03-30", 4),
            ("2026-10-24", 4),
            ("2026-10-25", 4),
            ("2026-10-26", 4),
            ("2026-03-29", 4),
            ("2026-03-29", 1),
            ("2026-03-29", 0),
            ("2026-03-29", 90),
        ] {
            cache.refresh(&windows, date(today), days);
            assert_marks(&cache, &windows);
        }
    }

    #[test]
    fn config_changes_invalidate_dates_and_scope() {
        let mut windows = windows();
        let mut cache = MaintenanceCalendar::default();
        let today = date("2026-03-30");
        cache.refresh(&windows, today, 3);
        windows[0].monitors.clear();
        cache.refresh(&windows, today, 3);
        assert_marks(&cache, &windows);
        // Replace recurring timing with fixed timing without changing the count.
        windows[0] = windows[1].clone();
        cache.refresh(&windows, today, 3);
        assert_marks(&cache, &windows);
        windows.remove(0);
        cache.refresh(&windows, today, 3);
        assert_marks(&cache, &windows);
        windows.clear();
        cache.refresh(&windows, today, 3);
        assert_marks(&cache, &windows);
    }
}
