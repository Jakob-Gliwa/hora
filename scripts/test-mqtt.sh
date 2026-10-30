#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
container="hora-mqtt-test-$$"
trap 'docker rm -f "$container" >/dev/null 2>&1 || true' EXIT
# The test broker is isolated from existing brokers and bound to loopback only.
port="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
docker run -d --name "$container" -p "127.0.0.1:$port:1883" \
  -v "$PWD/scripts/tests/mosquitto.conf:/mosquitto/config/mosquitto.conf:ro" \
  eclipse-mosquitto:2 >/dev/null
export HORA_TEST_MQTT_URL="mqtt://127.0.0.1:$port"
export HORA_TEST_MQTT_CONTAINER="$container"
cargo test -p hora-core --test mqtt_mosquitto -- --ignored --nocapture
