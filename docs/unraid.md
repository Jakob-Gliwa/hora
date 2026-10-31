# Running the fork on Unraid

GitHub Actions publishes `ghcr.io/jakob-gliwa/hora:main` after the tests and
security checks pass. The image targets `linux/amd64` for Unraid and builds natively without QEMU.
Docs changes do not trigger image builds; no GitHub Pages deployment runs.
CI compiles the release binary natively with Rust 1.99.1 and persists Cargo
registry and build artifacts using rust-cache. Docker receives only that binary,
so its context cannot contain private configuration. Local `docker build .`
still compiles from source.
For a fixed version, use `ghcr.io/jakob-gliwa/hora:sha-<full-commit-sha>`.
The image contains only the application, never installation configuration.

In Unraid, use **Docker → Add Container**:

| Field | Value |
| --- | --- |
| Name | `hora` |
| Repository | `ghcr.io/jakob-gliwa/hora:main` |
| Network type | Bridge |
| Container port | `8787` TCP; choose an unused host port |
| Config file mapping | Your private `config.toml` → `/etc/hora/config.toml`, read-only |
| Data directory mapping | Your appdata directory → `/data`, read/write |
| WebUI | `http://[IP]:[PORT:8787]/` |
| Variable | `TOKIO_WORKER_THREADS=2` (image default) |

The process runs as UID/GID `10001:10001`. Create the data directory with that
owner and ensure that UID can read the config file. Keep the database on local
storage, not an SMB/NFS mount. Back up `/data` and your private configuration.
The image overrides the bind address and database path to `0.0.0.0:8787` and
`/data/hora.db`, so a migrated config's workstation path is not used.

Set the variables listed in your private migration `environment.example` in
Unraid, or use a local owner-readable environment file via the advanced Extra
Parameters field: `--env-file /your/private/path/hora.env`. This path is on the
Unraid host. Do not upload the file, config or exports to GitHub. Keep access to
Unraid's saved container templates restricted, since explicit variable values
are saved there. The dedicated Hora watchdog needs its own `HORA_HEARTBEAT_URL`.

ICMP uses unprivileged datagram sockets. If the host disallows them, add
`--sysctl net.ipv4.ping_group_range="0 2147483647"` to Extra Parameters.
Privileged mode is not needed. HTTP/DNS/TCP/MQTT checks use the container's
network, so configure addresses reachable from Unraid (not workstation localhost).

After starting, inspect the container health and open the WebUI. To update the
moving `main` tag, use Unraid's Check for Updates / Update controls. Pin a commit
tag when you want explicit version selection; keep a database backup before
upgrading and restore the matching backup if a rollback crosses migrations.

## First GHCR publication

GitHub initially creates packages with private visibility. The repository owner
may need to open the `hora` package's settings and change its visibility to Public
once, so Unraid can pull without credentials. Only generic application code is
in the image. Alternatively keep it private and authenticate the Unraid host to
GHCR with a personal token scoped to `read:packages`; never put that token in
this repository or the image. A public source repository alone does not prove
that its container package is public.
