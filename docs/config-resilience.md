# Partial configuration loading

One faulty entry no longer stops Hora. The daemon loads `config.toml`
leniently: every monitor, channel, MQTT broker and maintenance window is
checked on its own, faulty ones are left out with an `ERROR` log line naming
the entry and the reason, and everything else starts. Ten monitors with one
typo start nine.

What a removal changes elsewhere is fixed rather than failed:

| Problem | Result |
| --- | --- |
| Monitor invalid (type error, unknown key, bad value, duplicate id) | Monitor not loaded (for duplicates the first one is kept) |
| Maintenance window names a monitor that is not loaded | The name is removed; a window left with **no** monitor is dropped, because an empty list means "every monitor" |
| `depends_on` names a monitor that is not loaded, or forms a cycle | That dependency is ignored |
| Channel invalid | Channel not loaded; routes to it are removed |
| MQTT broker invalid | Broker not loaded, and its MQTT monitors with it |
| `server.group_tokens` group without a loaded monitor | Token ignored |
| `[digest]` schedule invalid | Digest not loaded |

The same rules apply to live reloads: a monitor edited into an invalid state
stops until it is fixed. Hora never keeps running an older version of an entry
you have changed.

## The configuration monitor

The built-in monitor `hora-config` is down while anything was left out, with
every issue in its message, and alerts like any other monitor (one failure is
enough). It recovers on the next load without issues. It is fed by Hora only;
`/api/push/hora-config` is rejected. The id is reserved.

```toml
[config_monitor]          # optional
name = "Hora: Konfiguration gültig"
group = "Watchdog"
notify = ["pushover-high"] # default: every channel
# repeat_alert_secs = 21600
```

## What has no partial form

- **The file cannot be read, has a TOML syntax error, or a global section
  (`[server]`, `[alerts]`, `[[peers]]`) is invalid.** At start-up Hora does not
  exit (a container would restart it in a loop): it answers every request with
  `503` and the reason, and reads the file again every 5 s until it loads. On a
  live reload the monitors keep running as loaded before, and the
  configuration monitor reports that the file cannot be loaded.
- **No channel or no monitor could be loaded.** Hora cannot alert anybody
  itself, so it withholds its external heartbeat (`[health].heartbeat_url`) and
  the dead-man service raises the alarm.

`hora check` stays strict: it lists every issue and exits non-zero, so a
deployment can refuse a faulty file before restarting the daemon.
