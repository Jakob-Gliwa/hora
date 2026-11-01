# Shared MQTT monitors

```toml
[[brokers]]
name = "sensors"
url = "mqtt://broker.example:1883"
username = "monitor"
password = "${MQTT_PASSWORD}"
# keep_alive_secs = 15
# max_packet_kb = 64

[[monitors]]
id = "sensor"
name = "Sensor"
kind = "mqtt"
broker = "sensors"
topic = "devices/sensor/state"
keyword = "online"
interval_secs = 60
max_age_secs = 180
```

The client is mqtt-glide (MQTT **5**, QoS 1 subscriptions). The broker must
support MQTT 5. `mqtts://` uses validated TLS and the system trust roots;
username/password belong in their own fields, with passwords from environment
variables. Broker settings and subscriptions reload live.

Each named broker has one connection, regardless of monitor count. Duplicate
endpoint/username definitions are rejected; monitors share the broker name.
Only exact, configured topics are cached: no wildcard-driven key growth, no
payload history and no application queue. A topic holds one payload shared by
its monitors. `max_packet_kb` bounds MQTT packets (default 64 KiB, maximum 1 MiB).
`max_body_kb` can further bound an individual monitor's assertion input.

`keyword`/`keyword_invert`, JSONPath and `jq` assertions use the HTTP assertion
semantics. A missing message is DOWN. Set `max_age_secs` for periodic messages:
once its receive timestamp is too old the check is DOWN. For retained LWT
state, omit max age if the state remains valid until the publisher changes it.

Disconnects clear cached payloads. All affected monitors then fail through
their ordinary `fail_threshold`; reconnect and subscription restoration happen
automatically. A reconnect requires a fresh delivery (including retained replay).
A refused subscription fails its topic. Assertions run outside the cache lock.

JSONPath/jq monitors remember the content verdict of the last message they
checked. The same message is not parsed again on the next check. Each monitor
has its own verdict; changing its assertions starts a fresh evaluator. Every
delivery has a new identity, including deliveries after broker replacement.
Age, connection and subscription checks still run on every tick, and the
scheduler still records checks and counts failures at its configured cadence.
Keyword-only monitors evaluate directly, since caching tiny state strings does
not pay off. The verdict cache retains no payload or parsed JSON tree.

Run the synthetic release microbenchmark to compare cached and uncached
evaluation with a new message every 1, 5 or 50 checks:

```sh
cargo test -p hora-core --release --lib --locked benchmark_mqtt_content -- --ignored --nocapture
```

The benchmark includes delivery copies, warming and five samples with alternating
measurement order. It reports median nanoseconds per check for a keyword, scalar
jq, object jq and a 4 KiB array expression. These are assertion-path timings,
not whole-daemon CPU savings or a measurement of the Unraid server. A stream
with a new message on every check cannot benefit from result reuse.

Measured on native macOS arm64, Rust 1.99.1 release, five checks per delivery
(median of five samples; synthetic inputs only):

| Assertion | Payload | Uncached ns/check | Cached ns/check | Speedup |
| --- | ---: | ---: | ---: | ---: |
| Numeric jq | 3 B | 382.8 | 91.0 | 4.20x |
| Object jq | 29 B | 1333.5 | 282.7 | 4.72x |
| Array jq | 4011 B | 241035.8 | 48481.1 | 4.97x |

With a new message every check, the JSON cases measured 0.99–1.00x: no speedup.
Keyword checks stayed within about 1% of the uncached path. The content-cache
slot is 32 bytes per evaluator on this target, plus an error string on failure;
each stored message gains an 8-byte generation. These figures exclude existing
broker locks, network and database work, and are not Linux/amd64 measurements.

Run `scripts/test-mqtt.sh` for an isolated Docker Mosquitto integration test:
retained replay, periodic messages, jq, stale detection, connection count,
broker stop/start, config reload, removal and clean shutdown. No installation
broker or notification channel is used.
