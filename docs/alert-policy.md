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

Push monitors can set `push_poll_secs = 30` to evaluate heartbeat deadlines and
reminders every 30 seconds while keeping their long `interval_secs` deadline.
This is particularly useful for daily jobs. It does not change the heartbeat
expiry interval. Defaults preserve the existing polling cadence.
