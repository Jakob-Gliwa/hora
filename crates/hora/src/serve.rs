//! The monitor itself (plain `hora`): the supervisor, the background tasks and
//! the HTTP server, until a shutdown signal.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use anyhow::Context as _;
use hora_core::config;
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

/// Run the monitor: load config, open the database, start the supervisor and
/// background tasks, and serve the status page until a shutdown signal.
#[allow(clippy::too_many_lines)] // One linear start-up sequence; splitting it only scatters it.
pub(crate) async fn serve() -> anyhow::Result<()> {
    let config_path = config::path();
    let Some(initial) = load_initial(&config_path).await else {
        return Ok(()); // shut down while waiting for a loadable file
    };
    // Held until the process ends: `hora compact` rewrites the file and must
    // never do so under a running daemon (nor two daemons share one file).
    let _lock = hora_core::db::lock_exclusive(&initial.server.database_path)
        .context("locking the database")?;
    let store = hora_core::db::connect_with_settings(
        &initial.server.database_path,
        &initial.server.database,
    )
    .await
    .context("opening database")?;
    // The notifier client (no proxy); per-monitor probe clients are built by the
    // supervisor so each can carry its own proxy.
    let client = hora_core::http::client(None).context("building HTTP client")?;

    // A shutdown signal lets the background tasks stop cleanly (finishing their
    // current iteration) instead of being aborted when the runtime drops.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // The scheduler's liveness beacon: each monitor tick bumps it, and the
    // dead-man heartbeat and /healthz read it to tell a live scheduler from a
    // wedged one. Shared with the supervisor (writers) and the web layer (reader).
    let last_tick = Arc::new(AtomicU64::new(0));

    // The supervisor owns the live config + notification channels and reconciles
    // monitor tasks on reload; other components read through its handles.
    let handle = hora_core::supervisor::start(
        initial,
        config_path,
        store.clone(),
        client.clone(),
        Arc::clone(&last_tick),
        shutdown_rx.clone(),
    );
    let cert_task = hora_core::cert::spawn_watcher(
        store.clone(),
        handle.config.clone(),
        handle.notifier.clone(),
        client.clone(),
        shutdown_rx.clone(),
    );

    // Mutual surveillance: the outbound dead-man heartbeat. It self-gates on the
    // [health] section and reads it live, so it is always spawned (and activates if
    // [health] is added on reload). The inbound peer-watch tasks are owned and
    // hot-reloaded by the supervisor alongside the monitors.
    let heartbeat_task = hora_core::mesh::peer::spawn_heartbeat(
        handle.config.clone(),
        store.clone(),
        client.clone(),
        Arc::clone(&last_tick),
        Arc::clone(&handle.self_health),
        shutdown_rx.clone(),
    );

    // Per-vantage latency: poll the peers' /api/peer/monitors in the
    // background and share the snapshot with the web layer. Self-gating like
    // the heartbeat (no peers = a no-op per round).
    let vantage_map = hora_core::mesh::vantage::new_map();
    let vantage_task = hora_core::mesh::vantage::spawn_poller(
        handle.config.clone(),
        client.clone(),
        Arc::clone(&vantage_map),
        shutdown_rx.clone(),
    );

    let digest_task = hora_core::digest::spawn(
        store.clone(),
        handle.config.clone(),
        handle.notifier.clone(),
        shutdown_rx.clone(),
    );

    // Hora's own new releases, for the operator's page and channels.
    // Self-gating on `[updates] check`, read live.
    let updates = hora_core::updates::new_slot();
    let updates_task = hora_core::updates::spawn(
        store.clone(),
        handle.config.clone(),
        handle.notifier.clone(),
        client,
        Arc::clone(&updates),
        shutdown_rx.clone(),
    );

    // The built-in configuration monitor: down while the load dropped entries.
    let config_task =
        hora_core::config_status::spawn(store.clone(), handle.config.clone(), shutdown_rx.clone());
    // Hora watching itself: stale monitors, crashed tasks, failing channels.
    let self_task = hora_core::self_check::spawn(
        store.clone(),
        handle.config.clone(),
        handle.notifier.clone(),
        Arc::clone(&handle.self_health),
        shutdown_rx.clone(),
    );

    let prune_task =
        hora_core::maintenance::spawn_pruner(&store, handle.config.clone(), shutdown_rx);

    let bind = handle.config.borrow().server.bind.clone();
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    tracing::info!(
        "hora {} listening on http://{bind}",
        env!("CARGO_PKG_VERSION")
    );

    let state = hora_web::AppState::new(
        store,
        handle.config.clone(),
        Arc::clone(&last_tick),
        handle.notifier.clone(),
    )
    .with_vantage(vantage_map)
    .with_updates(updates);
    // Build the status summary while the server comes up, so the first page
    // view finds it ready instead of waiting for it.
    state.start_refresher();
    // Connect-info gives the rate limiter the peer socket IP: the client key
    // unless `server.client_ip_header` names a trusted proxy header (forwarded
    // headers are never read by default - a direct client could forge them).
    let app = hora_web::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("running HTTP server")?;

    // The HTTP server has drained; now stop the background tasks and wait briefly
    // for them to finish their current iteration before the runtime drops.
    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let _ = tokio::join!(
            handle.task,
            cert_task,
            prune_task,
            heartbeat_task,
            digest_task,
            vantage_task,
            updates_task,
            config_task,
            self_task
        );
    })
    .await;
    Ok(())
}

pub(crate) fn init_tracing() {
    // Distinguish "unset" (silent default) from "set but invalid" (warn, so a
    // typo'd filter isn't silently ignored). Tracing isn't up yet, so use stderr.
    let filter = match std::env::var("HORA_LOG") {
        Ok(value) => EnvFilter::try_new(&value).unwrap_or_else(|err| {
            eprintln!("warning: invalid HORA_LOG {value:?} ({err}); using info");
            EnvFilter::new("info")
        }),
        Err(_) => EnvFilter::new("info"),
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// How often a configuration that cannot be loaded at all is read again.
const CONFIG_RETRY: Duration = Duration::from_secs(5);

/// Load the configuration for the daemon through the lenient loader: faulty
/// entries are dropped and logged, the rest starts. A file that cannot be
/// loaded at all (unreadable, TOML syntax, invalid global settings) does not
/// end the process - a container would only restart it in a loop: Hora
/// answers every request with 503 and the reason, and reads the file again
/// every [`CONFIG_RETRY`] until it loads. `None` = shut down while waiting.
async fn load_initial(path: &std::path::Path) -> Option<config::Config> {
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let reason = Arc::new(std::sync::Mutex::new(String::new()));
    let mut error_page = None;
    loop {
        match config::load_lenient_from(path) {
            Ok(config) => {
                stop_error_page(error_page.take()).await;
                hora_core::supervisor::log_issues(&config);
                return Some(config);
            }
            Err(err) => {
                let message = format!("{err:#}");
                let changed = {
                    let mut current = reason
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let changed = *current != message;
                    if changed {
                        current.clone_from(&message);
                    }
                    changed
                };
                if changed {
                    tracing::error!(
                        "configuration cannot be loaded, retrying every {}s until it is fixed: {message}",
                        CONFIG_RETRY.as_secs()
                    );
                }
                if error_page.is_none() {
                    error_page = serve_config_error(Arc::clone(&reason)).await;
                }
            }
        }
        tokio::select! {
            () = tokio::time::sleep(CONFIG_RETRY) => {}
            () = &mut shutdown => {
                stop_error_page(error_page.take()).await;
                return None;
            }
        }
    }
}

type ErrorPage = (
    tokio::task::JoinHandle<()>,
    tokio::sync::oneshot::Sender<()>,
);

/// While waiting for a loadable configuration: 503 and the reason on every
/// path (so `/healthz` and the container health check fail visibly), bound
/// where the daemon will listen (`HORA_BIND`, else its default).
async fn serve_config_error(reason: Arc<std::sync::Mutex<String>>) -> Option<ErrorPage> {
    let bind = std::env::var("HORA_BIND").unwrap_or_else(|_| "127.0.0.1:8787".to_owned());
    let listener = match tokio::net::TcpListener::bind(&bind).await {
        Ok(listener) => listener,
        Err(err) => {
            tracing::warn!("cannot serve the configuration error page on {bind}: {err}");
            return None;
        }
    };
    let app = axum::Router::new().fallback(move || {
        let reason = Arc::clone(&reason);
        async move {
            let reason = reason
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "Hora cannot load its configuration:\n\n{reason}\n\nIt reads the file again \
                     every {}s and starts as soon as it loads.\n",
                    CONFIG_RETRY.as_secs()
                ),
            )
        }
    });
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await;
    });
    Some((task, stop))
}

/// Stop the error page and release its port before the real server binds.
async fn stop_error_page(page: Option<ErrorPage>) {
    if let Some((task, stop)) = page {
        let _ = stop.send(());
        let _ = task.await;
    }
}

/// Resolve when the process receives a shutdown signal. Listens for Ctrl-C on
/// every platform and, on Unix, also `SIGTERM` - the signal `docker stop` and
/// most init systems send - so the server drains in-flight requests cleanly
/// instead of being killed after the grace period.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!("failed to listen for Ctrl-C: {err}");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sigterm) => {
                sigterm.recv().await;
            }
            Err(err) => {
                tracing::error!("failed to listen for SIGTERM: {err}");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
    tracing::info!("shutting down");
}
