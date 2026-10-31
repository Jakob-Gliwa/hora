# Recurring maintenance

Fixed `start`/`end` windows keep their existing behavior. Alternatively:

```toml
[[maintenance]]
title = "Scheduled update"
cron = "0 3 * * 0"
timezone = "Europe/Berlin"
duration_secs = 900
monitors = ["website"]
```

Cron has five fields and uses the IANA timezone (default UTC), including DST.
The duration is elapsed seconds, from 1 second to 7 days. Recurring windows
are active from their start up to, excluding, their end. Empty `monitors`
means all monitors; explicit IDs are validated. Fixed timestamps and cron
cannot be combined. Checks continue; alerts are muted, and status cards,
maintenance announcements, daily history and reports show the window.

The status summary caches historical day marks once per window, shared by all
monitor cards. Repeated refreshes on the same UTC day reuse them; a new day only
computes the newly visible days. Changes to windows, monitor scope or history
length rebuild the cache, as do backward clock jumps. Memory is bounded by the
configured windows times displayed history days. Live maintenance and alert
suppression still evaluate the current time normally.
