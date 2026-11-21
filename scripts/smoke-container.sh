#!/usr/bin/env bash
# Synthetic, network-isolated runtime checks shared by the docker and publish
# jobs: the binary carries the repository's build settings, accepts the example
# config, runs as its own user, answers /healthz, the status page and its
# assets, and reports live-reload caveats only for a single-file mount.
# Usage: smoke-container.sh IMAGE
set -euo pipefail
image="${1:?usage: smoke-container.sh IMAGE}"
repo="$(cd "$(dirname "$0")/.." && pwd)"
config_dir="$(mktemp -d)"
directory_container="hora-smoke-$$"
file_container="hora-smoke-file-$$"
cleanup() {
  docker rm -fv "$directory_container" "$file_container" >/dev/null 2>&1 || true
  rm -rf "$config_dir"
}
trap cleanup EXIT
fail() {
  echo "container smoke test failed: $*" >&2
  docker logs "$directory_container" >&2 2>&1 || true
  docker logs "$file_container" >&2 2>&1 || true
  exit 1
}

docker run --rm --network none --entrypoint sh "$image" \
  -c 'grep -q "DEFAULT_MEMSTATUS=0" /usr/local/bin/hora' \
  || fail "LIBSQLITE3_FLAGS from .cargo/config.toml did not reach the binary"
docker run --rm --network none \
  -v "$repo/config.example.toml:/etc/hora/config.toml:ro" "$image" check >/dev/null \
  || fail "hora check rejects config.example.toml"

printf '[page]\n[server]\n' > "$config_dir/config.toml"
chmod 755 "$config_dir"
chmod 644 "$config_dir/config.toml"
docker run -d --name "$directory_container" --network none \
  -v "$config_dir:/etc/hora:ro" "$image" >/dev/null
docker run -d --name "$file_container" --network none \
  -v "$config_dir/config.toml:/etc/hora/config.toml:ro" "$image" >/dev/null

health=""
for _ in $(seq 1 30); do
  if health=$(docker exec "$directory_container" wget -qO- http://127.0.0.1:8787/healthz 2>/dev/null) \
    && docker logs "$file_container" 2>&1 | grep -q 'live reload is limited'; then
    break
  fi
  health=""
  sleep 1
done
[ -n "$health" ] || fail "no healthy /healthz answer or no single-file caveat within 30 s"
grep -q '"status":"ok"' <<<"$health" || fail "/healthz not ok: $health"
grep -q '"probes_ok":true' <<<"$health" || fail "/healthz lacks probes_ok=true: $health"
[ "$(docker exec "$directory_container" id -u)" = 10001 ] || fail "the daemon does not run as uid 10001"
docker exec "$directory_container" wget -qO /dev/null -U 'Mozilla/5.0' http://127.0.0.1:8787/ \
  || fail "the status page did not answer 200"
docker exec "$directory_container" wget -qO /dev/null http://127.0.0.1:8787/assets/hora.css \
  || fail "the stylesheet did not answer 200"
if docker logs "$directory_container" 2>&1 | grep 'live reload is'; then
  fail "live reload reported as limited on a directory mount"
fi
docker image ls "$image" --format 'image size: {{.Size}}'
echo "ok: $image"
