# Kuma-compatible push producers

Enable `kuma_push = true` on a push monitor and supply its existing token via
`push_token = "${JOB_PUSH_TOKEN}"`. GET and POST to
`/api/push/<token>?status=up|down&msg=...&ping=...` then identify that monitor
and return `{"ok":true}`. POST bodies are optional and ignored. Fractional
milliseconds are rounded to the nearest integer; empty ping is omitted.
Invalid status, negative/non-finite ping and unknown tokens are rejected.

The native POST `/api/push/{id}` with `X-Push-Token` remains available.
Tokens must be nonempty, URL-safe, unique and distinct from monitor/peer IDs.
Hora redacts push paths from its request logs and sets `Cache-Control: no-store`;
reverse proxies must also redact these paths in their own access logs.

Healthy push monitors sleep until a committed push or the next heartbeat
deadline. `interval_secs` remains the maximum heartbeat gap; cron schedules
retain their grace period and early-ping allowance. Deadlines are evaluated in
the first UTC second after the allowed interval, without periodic database
reads while healthy or still waiting for the first heartbeat. A push updates
the deadline and wakes only its monitor. SQLite stays authoritative; the wakeup
channel stores no message history and is removed when the task exits.

`push_poll_secs = 30` controls follow-up evaluations during failures/degradation,
reminders, or a recovery held back by maintenance/silence. Pending failures use
`retry_interval_secs` if configured. Repeated failure pushes cannot accelerate
the threshold or postpone its timer; recovery/status changes are handled as
they arrive. History continues to record misses at the follow-up cadence.
New pushes coalesce to the latest stored heartbeat, as with the previous poller.

Each idle task checks wall-clock corrections and updates scheduler liveness at
most a minute apart, without querying SQLite. Normally expiry fires at its exact
UTC deadline; a forward system-clock jump is noticed within that minute. Retry
intervals use monotonic time. Restart/reload reads the last heartbeat and the
persisted first-expected time, so a restart does not grant a fresh grace period.
Global configuration changes wake the task; database read errors retry within
30 seconds (or the shorter configured follow-up interval).
