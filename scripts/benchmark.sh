#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
data="$(python3 -c 'import pathlib,sys; print(pathlib.Path(sys.argv[1]).resolve())' "${1:-.local/benchmark}")"
seconds="${2:-120}"
broker="hora-benchmark-broker-$$"
fixtures="hora-benchmark-fixtures-$$"
monitor="hora-benchmark-monitor-$$"
cleanup() { docker rm -f "$monitor" "$fixtures" "$broker" >/dev/null 2>&1 || true; }
trap cleanup EXIT
test -f "$data/config.toml"
test -f "$data/fixtures.json"
# A single loopback namespace, no outbound network, no published ports.
docker run -d --name "$broker" --network none \
  -v "$PWD/scripts/tests/mosquitto.conf:/mosquitto/config/mosquitto.conf:ro" eclipse-mosquitto:2 >/dev/null
docker run -d --name "$fixtures" --network "container:$broker" \
  -v "$data:/data" -v "$PWD/scripts/benchmark-fixtures.py:/runner.py:ro" \
  python:3.13-slim-bookworm python /runner.py >/dev/null
docker run -d --name "$monitor" --network "container:$broker" --cap-add NET_RAW \
  -v "$data:/data" -e HORA_CONFIG=/data/config.toml -e RUST_LOG=error \
  -e TOKIO_WORKER_THREADS=2 hora-fork:benchmark >/dev/null
python3 scripts/measure-benchmark.py "$monitor" "$fixtures" "$data" "$seconds"
