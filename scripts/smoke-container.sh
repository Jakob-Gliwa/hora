#!/usr/bin/env bash
# Synthetic, network-isolated runtime checks shared by build and publish jobs.
set -euo pipefail
image="${1:?usage: smoke-container.sh IMAGE}"
config_dir="$(mktemp -d)"
directory_container="hora-smoke-$$"
file_container="hora-smoke-file-$$"
cleanup() {
  docker rm -fv "$directory_container" "$file_container" >/dev/null 2>&1 || true
  rm -rf "$config_dir"
}
trap cleanup EXIT
printf '[page]\n[server]\n' > "$config_dir/config.toml"
chmod 755 "$config_dir"
chmod 644 "$config_dir/config.toml"
docker run -d --name "$directory_container" --network none \
  -v "$config_dir:/etc/hora:ro" "$image" >/dev/null
docker run -d --name "$file_container" --network none \
  -v "$config_dir/config.toml:/etc/hora/config.toml:ro" "$image" >/dev/null
for attempt in $(seq 1 30); do
  if docker exec "$directory_container" wget -qO /dev/null http://127.0.0.1:8787/healthz \
    && docker logs "$file_container" 2>&1 | grep -q 'config: live reload: the file is mounted on its own'; then
    docker image ls "$image" --format 'image size: {{.Size}}'
    if docker logs "$directory_container" 2>&1 | grep 'config: live reload'; then
      echo "live reload reported as broken on a directory mount"; exit 1
    fi
    exit 0
  fi
  sleep 1
done
docker logs "$directory_container"
docker logs "$file_container"
echo "container smoke test failed"
exit 1
