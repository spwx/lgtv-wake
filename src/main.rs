mod cert;
mod config;
mod doctor;
mod lock;
mod marks;
mod pin;
mod power;
mod setup;
mod ssap;
mod tls;
mod tv;
mod update;
mod watch;
mod wol;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::config::Config;

/// Wake an LG webOS TV and switch inputs when a controller connects.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Pair with the TV (accept the prompt on screen) and save the client key
    Pair {
        /// Overwrite an existing client key
        #[arg(long)]
        force: bool,
    },
    /// Wake the TV if needed and switch to the configured input
    On,
    /// Turn the TV off if it's on the configured input
    Off,
    /// Print the TV's power state and current input
    Status,
    /// Fetch the TV's TLS certificate and pin it (after a firmware update changed it)
    Pin {
        /// Pin without asking
        #[arg(long)]
        yes: bool,
    },
    /// Check the config, client key, installed files and the TV connection
    Doctor,
    /// Disconnect the controllers and turn the TV off unless the system is rebooting (run by the sleep and power-off hooks)
    SystemOff,
    /// Install the binary, config, systemd units, udev rule and sleep hook, then pair (Linux only)
    Setup(setup::Options),
    /// Download the latest release and run its setup (Linux only)
    Update(update::Options),
    /// Watch a controller, keyboard or mouse event device and drive the TV (Linux only)
    Watch {
        /// Event device, e.g. /dev/input/event17
        device: PathBuf,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        // The command printed its own message (e.g. `status` with the TV off).
        Err(e) if e.is::<tv::Reported>() => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("Error: {e:?}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        // No colour escapes when logging to the journal or a file.
        .with_ansi(std::io::stderr().is_terminal())
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    #[cfg(not(target_os = "linux"))]
    if let Command::Watch { .. } = cli.command {
        anyhow::bail!("watch is only supported on Linux");
    }

    if let Command::Setup(opts) = &cli.command {
        return setup::run(opts).await;
    }
    if let Command::Update(opts) = &cli.command {
        return update::run(opts);
    }
    if let Command::Doctor = cli.command {
        return doctor::run().await;
    }

    let cfg = Config::load()?;

    match cli.command {
        Command::Pair { force } => tv::pair(&cfg, force).await,
        Command::On => tv::on(&cfg).await,
        Command::Off => tv::off(&cfg).await,
        Command::Status => tv::status(&cfg).await,
        Command::Pin { yes } => pin::run(&cfg, yes).await,
        Command::SystemOff => power::run(&cfg).await,
        Command::Setup(_) | Command::Update(_) | Command::Doctor => unreachable!(),
        #[cfg(target_os = "linux")]
        Command::Watch { device } => watch::run(&cfg, &device).await,
        #[cfg(not(target_os = "linux"))]
        Command::Watch { .. } => unreachable!(),
    }
}
