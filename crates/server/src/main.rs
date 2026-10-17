//! `aiworkercontrol`: serves the dashboard on the loopback interface.
//!
//! ```text
//! aiworkercontrol [--root DIR] [--port PORT]
//! ```
//!
//! `--root` is the directory holding `work/`, `spec/` and
//! `aiworkercontrol.toml` (default: the current directory); `--port`
//! defaults to 8787.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use anyhow::{Context, bail};

const DEFAULT_PORT: u16 = 8787;
/// How often merged worktrees are looked for.
const CLEANUP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (root, port) = parse_args(std::env::args().skip(1))?;
    let root =
        std::path::absolute(&root).with_context(|| format!("resolving {}", root.display()))?;
    if !root.join("work").is_dir() {
        bail!("{} has no work/ directory; pass --root", root.display());
    }

    let state = server::AppState::new(&root, port)?;
    // Loopback only: the app runs agents with shell access and holds API keys.
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    println!(
        "AI Worker Control: http://{addr}/ (root {})",
        root.display()
    );
    tokio::spawn(clean_up_merged(state.clone()));
    axum::serve(listener, server::router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    // Returning drops the runtime and with it every worker's run, which
    // stops each agent's whole process group. Items left in progress go back
    // to open on the next start.
    Ok(())
}

/// Resolves on Ctrl-C or SIGTERM, so the server exits normally and its
/// agents are stopped rather than left running unsupervised.
async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut term) => {
                term.recv().await;
            }
            // Without a SIGTERM handler, Ctrl-C still works.
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = ctrl_c => {}
        () = term => {}
    }
    println!("shutting down: stopping agents");
}

/// Every few minutes, frees the worktrees of closed items whose branches
/// have been merged.
async fn clean_up_merged(state: server::AppState) {
    let mut every = tokio::time::interval(CLEANUP_INTERVAL);
    loop {
        every.tick().await;
        match state.pool().clean_up_merged().await {
            Ok(items) => {
                for item in items {
                    println!("cleaned up merged {item}");
                }
            }
            Err(e) => eprintln!("worktree cleanup: {e}"),
        }
    }
}

fn parse_args(mut args: impl Iterator<Item = String>) -> anyhow::Result<(PathBuf, u16)> {
    let mut root = PathBuf::from(".");
    let mut port = DEFAULT_PORT;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => root = args.next().context("--root needs a directory")?.into(),
            "--port" => {
                port = args
                    .next()
                    .context("--port needs a number")?
                    .parse()
                    .context("--port needs a number")?;
            }
            "-h" | "--help" => {
                println!("usage: aiworkercontrol [--root DIR] [--port PORT]");
                std::process::exit(0);
            }
            other => bail!("unknown argument {other}; see --help"),
        }
    }
    Ok((root, port))
}
