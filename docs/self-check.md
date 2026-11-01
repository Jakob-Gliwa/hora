# Self-check

The external dead-man heartbeat (`[health].heartbeat_url`) catches a dead
process, a wedged scheduler and a broken database. The built-in monitor
`hora-self` covers what the heartbeat cannot see. Every minute it looks for:

- **A monitor that stopped checking**: no check for more than three of its
  periods (interval or timeout, whichever is longer) plus a minute. Push
  monitors watch their own deadline and are skipped; a monitor that was just
  added gets the same grace before it counts.
- **A crashed monitor or peer-watch task** within the last hour. Hora restarts
  such tasks on its own; the self-check makes a crash loop visible.
- **A failing notification channel**: at least `[alerts].channel_fail_threshold`
  failed deliveries in a row.
- **A stopped alert coalescer**, after which no down or recovered alert is
  delivered until restart.

While anything is found, `hora-self` is down with every finding in its message
and alerts like any other monitor. It recovers on its own. It is fed by Hora
only; `/api/push/hora-self` is rejected and the id is reserved.

```toml
[self_monitor]            # optional
name = "Hora: Selbstcheck"
group = "Watchdog"
# notify = ["pushover-high"]   default: every channel
# repeat_alert_secs = 21600
```

## When no alert can get out

If **every** notification channel is failing (for example several channels on
one Pushover account whose token was revoked), or the coalescer has stopped,
an alert from Hora reaches nobody. Hora then withholds its external heartbeat,
so the dead-man service raises the alarm through its own channels. The
heartbeat resumes once a delivery succeeds again.
