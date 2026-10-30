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

For long heartbeat deadlines, set `push_poll_secs = 30` so expiry is evaluated
promptly. `interval_secs` remains the maximum heartbeat gap.
