// SPDX-License-Identifier: GPL-3.0-or-later

//! ameleo-display-agent: Linux guest agent speaking peer-to-peer D-Bus over
//! the virtio-serial port `fr.ameleo.agent.0`.
//!
//! ```text
//! ameleo-display-agent daemon    system daemon (root): the port, the pointer
//! ameleo-display-agent session   session agent (the user): clipboard, files, effects
//! ```

mod channel;
mod daemon;
mod ipc;
mod session;

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("AMELEO_DISPLAY_AGENT_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info"));
    // journald adds its own timestamps.
    let journald = std::env::var_os("JOURNAL_STREAM").is_some();
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(!journald && rustix::termios::isatty(std::io::stderr()));
    if journald {
        builder.without_time().init();
    } else {
        builder.init();
    }
}

fn main() -> anyhow::Result<()> {
    let arg = std::env::args().nth(1).unwrap_or_default();
    let role = match arg.as_str() {
        "daemon" | "session" => arg,
        "--version" | "-V" => {
            println!("ameleo-display-agent {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        _ => {
            eprintln!("usage: ameleo-display-agent daemon|session|--version");
            std::process::exit(2);
        }
    };
    init_logging();
    tracing::info!(
        "ameleo-display-agent {} starting ({role})",
        env!("CARGO_PKG_VERSION")
    );
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let res = rt.block_on(async {
        if role == "daemon" {
            daemon::run().await
        } else {
            session::run().await
        }
    });
    if let Err(e) = &res {
        tracing::error!("{role} failed: {e:#}");
    }
    res
}
