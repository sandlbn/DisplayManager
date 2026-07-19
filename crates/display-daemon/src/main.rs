//! `displayd` — owns all hardware access; GUI, CLI and API are thin clients.

use display_api::protocol::{Request, Response, INTERNAL_ERROR};
use display_daemon::{rpc, worker};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("displayd requires macOS");
        std::process::exit(1);
    }

    #[cfg(target_os = "macos")]
    {
        // displayd is a per-user agent. Run as root (via sudo) and the socket
        // it creates is root-owned, so the user's GUI and CLI — which run
        // unprivileged — get EACCES and report "displayd not running". DDC needs
        // no elevation, so root is never the fix. Warn loudly rather than fail:
        // a determined operator may have a reason, but the default confusion is
        // worth heading off.
        if unsafe { libc::geteuid() } == 0 {
            tracing::warn!(
                "running as root — the socket will be root-owned and unprivileged \
                 clients (the menu bar app, displayctl) will be unable to connect. \
                 DDC does not require root; run displayd as your normal user."
            );
        }

        let backend = match display_macos::MacosBackend::new() {
            Ok(b) => b,
            Err(e) => {
                tracing::error!("backend unavailable: {e}");
                std::process::exit(1);
            }
        };
        let worker = worker::spawn(backend);

        let path = display_api::socket_path();
        let listener = match bind(&path) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("cannot bind {}: {e}", path.display());
                std::process::exit(1);
            }
        };
        tracing::info!("displayd listening on {}", path.display());

        tokio::select! {
            _ = serve(listener, worker.clone()) => {}
            _ = automation_loop(worker) => {}
            _ = shutdown_signal() => {
                tracing::info!("shutting down");
            }
        }

        // The socket file outlives the process otherwise, and the next start
        // would fail with EADDRINUSE against a socket nothing is listening on.
        let _ = std::fs::remove_file(&path);
    }
}

/// How often to evaluate automation rules.
///
/// A compromise: fast enough that plugging in a dock feels responsive, slow
/// enough that the I2C worker is mostly free for real requests. Each tick costs
/// one CoreGraphics call and one power query when nothing changed — the
/// expensive work only happens on an actual edge. Proper hot-plug notifications
/// (`CGDisplayRegisterReconfigurationCallback`) would let this go away.
const AUTOMATION_INTERVAL: Duration = Duration::from_secs(5);

/// Drive rule evaluation on a timer.
async fn automation_loop(worker: worker::WorkerHandle) {
    let mut ticker = tokio::time::interval(AUTOMATION_INTERVAL);
    // If a tick is slow (a monitor retrying), skip the backlog rather than
    // queueing evaluations that would each re-check the same state.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;
        match worker.send(worker::Command::AutomationTick).await {
            Ok(fired) => {
                for f in fired {
                    if !f.ok {
                        tracing::warn!("rule {:?}: {}", f.name, f.detail);
                    }
                }
            }
            // Never break the loop: a transient failure must not silently
            // disable automation for the rest of the session.
            Err(e) => tracing::warn!("automation tick failed: {e}"),
        }
    }
}

/// Wait for a signal that means "stop".
///
/// SIGTERM matters more than SIGINT here: launchd stops a LaunchAgent with
/// SIGTERM, so handling only ctrl-c would mean the socket is never cleaned up in
/// the way the daemon actually runs in production.
#[cfg(target_os = "macos")]
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("cannot listen for SIGTERM: {e}");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = term.recv() => tracing::info!("received SIGTERM"),
        _ = tokio::signal::ctrl_c() => tracing::info!("received SIGINT"),
    }
}

/// Bind the socket, clearing a stale one left by an unclean exit.
fn bind(path: &PathBuf) -> std::io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Only remove a socket that nothing answers on — never clobber a live daemon.
    if path.exists() {
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    "another displayd is already listening",
                ))
            }
            Err(_) => {
                tracing::warn!("removing stale socket {}", path.display());
                std::fs::remove_file(path)?;
            }
        }
    }
    let listener = UnixListener::bind(path)?;
    // Owner-only: a per-user control socket has no reason to accept connections
    // from other local accounts.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

async fn serve(listener: UnixListener, worker: worker::WorkerHandle) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let w = worker.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle(stream, w).await {
                        tracing::debug!("connection ended: {e}");
                    }
                });
            }
            Err(e) => {
                tracing::error!("accept failed: {e}");
                return;
            }
        }
    }
}

/// One connection: newline-framed JSON-RPC, one response per request.
async fn handle(stream: UnixStream, worker: worker::WorkerHandle) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => rpc::dispatch(&worker, req).await,
            // Unparseable frame: no id to correlate, so answer with 0 rather
            // than dropping the client without explanation.
            Err(e) => Response::err(0, INTERNAL_ERROR, format!("bad request: {e}")),
        };
        let mut out = serde_json::to_string(&resp).unwrap_or_else(|_| {
            r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32603,"message":"serialize failed"}}"#
                .to_string()
        });
        out.push('\n');
        write.write_all(out.as_bytes()).await?;
        write.flush().await?;
    }
    Ok(())
}
