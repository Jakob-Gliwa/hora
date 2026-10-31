# Monitor alert policy

`fail_threshold` on a monitor overrides `[alerts].fail_threshold`. It counts
consecutive recorded failures (1..1000). `probe_retries` remains the separate,
immediate retry policy; migrations use `probe_retries = 0` and convert Kuma's
`maxretries` to `fail_threshold = maxretries + 1`.

`retry_interval_secs` changes the next check delay while failures are pending
confirmation. After confirmation or recovery the ordinary cadence resumes.
Omit it to preserve Hora's fixed cadence.

`repeat_alert_secs` in `[alerts]` enables periodic DOWN reminders. Each monitor
can override it, including with 0 to disable reminders. Only confirmed outages
repeat. Maintenance, silences, local-only failures and topology grouping still
suppress reminders. Recovery resets the timer; reload preserves it, and the
last reminder time is stored in SQLite for daemon restarts. Reminder delivery
uses the existing channel retry/watchdog behavior. A missed period produces one
reminder at the next evaluation, never a burst.

Push monitors use events and deadlines while healthy: they read the latest
heartbeat on startup, after a push, on configuration changes, or at expiry.
`push_poll_secs = 30` retains a 30-second follow-up cadence while failed,
degraded or waiting to announce a muted recovery. Its default remains
`interval_secs`. Pending failures still use `retry_interval_secs`, and repeated
failure pushes cannot accelerate the threshold. Maintenance/silence, burn-rate
checks, reminder timing and peer reconfirmation use the existing alert state
machine. See [push scheduling](kuma-push.md) for clock and restart behavior.
