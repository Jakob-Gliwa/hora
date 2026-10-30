# Pushover channel overrides

Each `type = "pushover"` channel accepts `priority` (-2 through 2) and
`sound` (for example `"none"`). Omit them to retain event-based priorities
and the recipient's sound preference. A configured priority applies to all
events routed to that channel, including recoveries.

Emergency priority 2 requires `retry` (at least 30 seconds) and `expire`
(1 through 10800 seconds). These fields are rejected at other priorities.
They control Pushover's acknowledgement retries, independently of Hora's
outage reminders. See the [Pushover API](https://pushover.net/api#priority).
