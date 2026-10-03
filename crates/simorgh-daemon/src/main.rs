//! `simorghd` — the headless Simorgh engine daemon.
//!
//! A normal long-lived process that owns the engine (the `zray-mobile`
//! INSTANCE lifecycle) and the `zero-discovery` jobs, and exposes both over a
//! loopback HTTP/JSON RPC so a native GUI can drive the full feature set
//! without touching a TUI or JNI. The contract it mirrors is
//! `native-contract.md` (the Kotlin↔Rust boundary of the upstream Android
//! app); the HTTP surface is documented in `docs/rpc-contract.md`.
//!
//! Start-up order matters and is fixed:
//!
//! 1. create the data directory, install file logging (rotated `simorgh.log`,
//!    level from `SIMORGH_LOG`, default `warn`);
//! 2. install the no-op socket protector — a desktop process is not behind
//!    its own tunnel, so no socket needs exempting, but installing one
//!    explicitly documents that choice instead of leaving it implicit;
//! 3. bind the loopback listener and mint the bearer token;
//! 4. print exactly one stdout handshake line and flush:
//!
//!    ```text
//!    SIMORGH_READY {"port":53123,"token":"…"}
//!    ```
//!
//! Everything after that speaks HTTP. Stderr stays free for a fatal error
//! before the handshake; stdout carries nothing else, ever.

mod engine;
mod http;
mod jobs;
mod logging;

use std::io::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use tokio::net::TcpListener;
use zero_core::SocketProtector;

/// The daemon does what the mobile host's `SocketProtection.protect` does
/// when no `VpnService` is live: nothing successfully. The process opens its
/// own sockets and sits behind no tunnel of its own, so there is no loop to
/// break.
struct NoOpProtector;

impl SocketProtector for NoOpProtector {
    fn protect(&self, _fd: i32) -> std::io::Result<()> {
        Ok(())
    }
}

const USAGE: &str = "\
simorghd — the Simorgh engine daemon

usage: simorghd [--data-dir DIR] [--listen 127.0.0.1:PORT] [--token-file FILE]

  --data-dir DIR     logs (simorgh.log), feed caches and managed assets go
                     here. Default: ~/Library/Application Support/Simorgh
                     (macOS) or ~/.simorgh.
  --listen ADDR:PORT loopback address to serve the RPC on. Default
                     127.0.0.1:0 — a random ephemeral port. Any non-loopback
                     address is refused.
  --token-file FILE  also write the bearer token to FILE (0600). The token is
                     always printed on the SIMORGH_READY line; a GUI that
                     spawns simorghd can read it there instead.

environment:
  SIMORGH_LOG        off|error|warn|info|debug|trace (default warn)

the RPC surface and its JSON schemas: docs/rpc-contract.md";

struct Args {
    data_dir: PathBuf,
    listen: SocketAddr,
    token_file: Option<PathBuf>,
}

fn default_data_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    if cfg!(target_os = "macos") {
        home.join("Library")
            .join("Application Support")
            .join("Simorgh")
    } else {
        home.join(".simorgh")
    }
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut data_dir: Option<PathBuf> = None;
    let mut listen: Option<SocketAddr> = None;
    let mut token_file: Option<PathBuf> = None;
    let mut argv = std::env::args().skip(1);
    while let Some(argument) = argv.next() {
        let mut value = |name: &str| -> Result<String, String> {
            argv.next().ok_or_else(|| format!("{name} needs a value"))
        };
        match argument.as_str() {
            "--data-dir" => data_dir = Some(PathBuf::from(value("--data-dir")?)),
            "--listen" => {
                let text = value("--listen")?;
                let parsed: SocketAddr = text
                    .parse()
                    .map_err(|error| format!("--listen {text:?} is not an ADDR:PORT: {error}"))?;
                // The RPC drives the whole engine; it must never reach past
                // this machine. (There is no tokenless mode to protect here:
                // a token is always generated.)
                if !parsed.ip().is_loopback() {
                    return Err(format!(
                        "--listen must be a loopback address, {parsed:?} is not"
                    ));
                }
                listen = Some(parsed);
            }
            "--token-file" => token_file = Some(PathBuf::from(value("--token-file")?)),
            "--help" | "-h" => return Ok(None),
            other => return Err(format!("unknown argument {other:?}; try --help")),
        }
    }
    Ok(Some(Args {
        data_dir: data_dir.unwrap_or_else(default_data_dir),
        listen: listen.unwrap_or(SocketAddr::new(
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            0,
        )),
        token_file,
    }))
}

fn main() -> std::process::ExitCode {
    let args = match parse_args() {
        Ok(Some(args)) => args,
        Ok(None) => {
            println!("{USAGE}");
            return std::process::ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("simorghd: {error}");
            return std::process::ExitCode::from(2);
        }
    };

    if let Err(error) = std::fs::create_dir_all(&args.data_dir) {
        eprintln!(
            "simorghd: cannot create {}: {error}",
            args.data_dir.display()
        );
        return std::process::ExitCode::FAILURE;
    }

    let level = std::env::var("SIMORGH_LOG").unwrap_or_else(|_| "warn".to_string());
    let level = match logging::parse_level(&level) {
        Ok(level) => level,
        Err(error) => {
            eprintln!("simorghd: SIMORGH_LOG: {error}");
            return std::process::ExitCode::from(2);
        }
    };
    if let Err(error) = logging::init(&args.data_dir, level) {
        // Logging is best effort, but a daemon nobody can read the mind of is
        // worse than one that says so on stderr.
        eprintln!("simorghd: {error}");
        return std::process::ExitCode::FAILURE;
    }

    // Once per process; a second installation (a library host that got here
    // first) is its own business, and `set_socket_protector` says so only in
    // the log.
    if let Err(reason) = zero_core::set_socket_protector(Arc::new(NoOpProtector)) {
        tracing::warn!(%reason, "socket protector not installed");
    }

    tracing::info!(data_dir = %args.data_dir.display(), ?level, "simorghd starting");

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("simorgh-http")
        .enable_all()
        .build();
    let runtime = match runtime {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("simorghd: could not start the HTTP runtime: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(args)) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "simorghd stopped");
            eprintln!("simorghd: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<(), String> {
    let listener = TcpListener::bind(args.listen)
        .await
        .map_err(|error| format!("could not bind {}: {error}", args.listen))?;
    let addr = listener
        .local_addr()
        .map_err(|error| format!("could not read the bound address: {error}"))?;

    // One token per process; the READY line is its delivery. A token file is
    // for hosts that would rather read a 0600 file than parse stdout.
    let token = uuid::Uuid::new_v4().simple().to_string();
    if let Some(path) = &args.token_file {
        write_token_file(path, &token)?;
    }

    println!(
        "SIMORGH_READY {}",
        json!({"port": addr.port(), "token": token})
    );
    std::io::stdout()
        .flush()
        .map_err(|error| format!("cannot write the ready line: {error}"))?;
    tracing::info!(%addr, "simorghd serving the RPC");

    let state = Arc::new(http::State {
        token,
        data_dir: args.data_dir,
    });
    let serving = http::serve(listener, Arc::clone(&state));
    wait_for_shutdown(serving).await;

    // Best effort: the engine outliving the RPC by a moment is confusing,
    // and a half-dead runtime would keep the user's ports bound.
    let stopped = tokio::task::spawn_blocking(engine::stop).await;
    match stopped {
        Ok(Ok(())) => tracing::info!("simorghd stopped the engine on the way out"),
        Ok(Err(_)) => {} // nothing was running; that is the contract's error
        Err(_) => tracing::warn!("the engine stop was interrupted"),
    }
    Ok(())
}

async fn wait_for_shutdown<F>(serving: F)
where
    F: std::future::Future<Output = std::io::Result<()>>,
{
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminated = signal(SignalKind::terminate())
            .map_err(|error| tracing::warn!(%error, "no SIGTERM handler"))
            .ok();
        tokio::select! {
            result = serving => {
                if let Err(error) = result {
                    tracing::error!(%error, "the accept loop ended");
                }
            }
            _ = tokio::signal::ctrl_c(), if true => {}
            _ = async {
                if let Some(signal) = terminated.as_mut() {
                    signal.recv().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {}
        }
    }
    #[cfg(not(unix))]
    {
        tokio::select! {
            result = serving => {
                if let Err(error) = result {
                    tracing::error!(%error, "the accept loop ended");
                }
            }
            _ = tokio::signal::ctrl_c() => {}
        }
    }
}

/// Write the bearer token, and only the token, so a host can read it without
/// the READY line. Mode 0600 on unix: it is a credential for driving the
/// whole engine.
#[cfg(unix)]
fn write_token_file(path: &std::path::Path, token: &str) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    file.write_all(token.as_bytes())
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    // `mode` only shapes a newly created file; an existing one keeps its bits.
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    Ok(())
}

#[cfg(not(unix))]
fn write_token_file(path: &std::path::Path, token: &str) -> Result<(), String> {
    std::fs::write(path, token.as_bytes())
        .map_err(|error| format!("cannot write {}: {error}", path.display()))
}
