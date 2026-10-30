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

Run `scripts/test-mqtt.sh` for an isolated Docker Mosquitto integration test:
retained replay, periodic messages, jq, stale detection, connection count,
broker stop/start, config reload, removal and clean shutdown. No installation
broker or notification channel is used.
