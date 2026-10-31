# Resource budget and isolated measurement

MQTT monitors share one mqtt-glide connection per broker and retain one bounded
payload per exact topic. Identical jq expressions share compiled bytecode,
including across config reloads; removed expressions are held only weakly.
There is no JavaScript runtime in the daemon.

SQLite's cache budget is per connection. The migration selects this smaller
budget; existing configurations keep the upstream defaults of 16000 KiB and
eight connections:

```toml
[server.database]
cache_kib = 2048
max_connections = 4
```

These two settings require a restart. They bound the SQLite page caches to
approximately 8 MiB in total, not the whole process. Query buffers, cached
summaries, TLS, compiled expressions, MQTT payloads and the runtime also use
memory. For a small installation, start with `TOKIO_WORKER_THREADS=2` and measure
before adding worker threads. The release build retains size optimization and
LTO to keep the executable's resident code small.

The benchmark harness remaps a normalized export to local HTTP/TCP/DNS/ICMP,
MQTT and push fixtures. It preserves expressions, intervals, retries, groups of
probe kinds, retention and maintenance timing. All notifications and external
release/certificate checks are disabled. HTTPS becomes local HTTP, so this does
not measure TLS handshakes or real network latency. Credentials and names are
replaced in the local benchmark config. Output files belong in `.local/`.

```sh
docker build -f scripts/Dockerfile.benchmark -t hora-fork:benchmark .
python3 scripts/prepare-benchmark.py /private/export.json \
  --cases /private/reference-cases.json --mqtt-max-age 42=300
scripts/benchmark.sh .local/benchmark 150
```

The case file contains `expression`, `expected`, optional `operator`, `body`
and `up` entries; provide passing samples for each JSON assertion. The synthetic
format is demonstrated in `scripts/tests/migration_cases.py`. Repeat the freshness
option for each periodic topic. A custom build CA can be supplied with Docker's
`--secret id=build_ca,src=/path/to/ca.pem`; it is not copied into the image.

The harness runs a Linux/glibc release build, Mosquitto and a Python fixture
server in one loopback-only network namespace with no outbound route or
published ports. It samples the Hora process's `/proc/1/status` RSS/high-water
mark and `/proc/1/stat` CPU time every two seconds. Fixture processes are separate
and are excluded from Hora's RSS. `metrics.json` reports aggregate values and
the number of monitors that actually recorded checks. The production Dockerfile
continues to use a static musl build; its footprint can differ.

A short run on a fresh database is an initial capacity check. Long-running
production memory, TLS traffic and historical-data query costs should also be
measured during the parallel trial.
