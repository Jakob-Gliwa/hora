# Migrating a normalized Kuma export

The migration script accepts the normalized JSON export with `monitors`,
`notification_channels`, `maintenance_windows` and `settings` sections. It is
separate from the existing `hora import kuma` command for original Kuma backups.
Python 3.9 or newer is needed only during conversion; the daemon remains Rust.

```sh
python3 scripts/migrate-kuma.py /private/path/export.json \
  --output-dir .local/migration \
  --mqtt-max-age 42=300
```

Repeat `--mqtt-max-age ID=SECONDS` for periodic MQTT publishers. Omit it for
retained state that stays valid until changed. Choose the age from the expected
publication cadence and tolerated delay. Unknown fields, unsupported types,
inactive monitors and unsupported expressions stop the conversion and appear
in `report.json`; a successful conversion has an empty `unmapped` list.

The generated files have owner-only permissions. Names, groups and descriptions
stay in the private config. The script extracts passwords, push tokens,
authorization headers and URL credentials into environment placeholders. It
never prints source values. `.local/`, `.env*` and `hora-fork-spec/` are excluded
from Git and the Docker build context. Do not publish exports or generated files.
Redaction markers such as `<REDACTED-UUID>` and their URL-encoded forms become
environment references too. Any leftover redaction marker stops conversion.

For an HTTP watchdog that pings an external dead-man service, pass
`--watchdog-monitor ID`. That monitor becomes `[health].heartbeat_url`, with
its interval retained and the URL replaced by `${HORA_HEARTBEAT_URL}`. Create a
**new, separate check for Hora** and put its complete ping URL in that variable;
never reuse Kuma's check during parallel operation, even if the export contains
its real URL. Configure the new check's expected period, grace time and alert
destinations at the external service. Monitor retry/notification policies do not
apply to this built-in heartbeat. Scoped or global maintenance affecting the
watchdog requires an explicit schedule decision and stops automatic conversion.

The built-in heartbeat sends an empty POST when Hora's scheduler and database
are healthy, and stays silent when they are not. It sends no monitor details to
the external service. `[health].heartbeat_url` is the external option;
`ping_url` belongs to a `[[peers]]` entry. The script prepares configuration only;
it does not create the external check or contact the service.

Fill the variables listed in `environment.example` locally using your existing
secrets. Load them with your service manager or a shell environment, then run:

```sh
HORA_CONFIG="$PWD/.local/migration/config.toml" cargo run --release -- check
HORA_CONFIG="$PWD/.local/migration/config.toml" cargo run --release
```

The default generated listener is loopback-only. All monitors require the new
`HORA_AUTH_TOKEN` for viewing, and automatic release checks are disabled. Before
running a parallel trial, decide which notification routes should be enabled to
avoid duplicate alerts alongside Kuma. Push producers must send to Hora too;
reuse their existing token values and the [compatible endpoint](kuma-push.md).
No history is imported and the script does not deploy or contact monitored hosts.

Mappings include accepted HTTP status ranges, headers, method, TLS policy,
keyword assertions, TCP, DNS resolver/record, ICMP packet count/size, Pushover
priority/sound, per-monitor retries, reminder cadence and timezone-aware cron
maintenance. `maxretries + 1` becomes `fail_threshold`; `resend_interval * interval`
becomes `repeat_alert_secs`. A missing notifier selection follows exported
channel defaults; an explicit empty selection remains empty.

JSONata translation is intentionally a checked subset: scalar thresholds,
boolean range checks, whitelist counts, indexed metric paths, numeric conversion,
half-to-even rounding with literal precision, conditional values and path-bound
counter regression checks. It is not a general JSONata implementation. Missing
metrics fail positive-value checks; missing regression samples produce no
regression. Explicit null is distinct from a missing field. Unsupported syntax
fails conversion. Review the generated jq when changing expressions.

Verification uses synthetic fixtures only:

```sh
python3 -m unittest discover -s scripts/tests -p 'test_*.py'
cargo test -p hora-core --test kuma_migration
scripts/test-mqtt.sh
```
