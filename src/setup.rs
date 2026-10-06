//! `setup`: install the binary, config, systemd user unit and udev rule (Linux).
//!
//! Safe to run again: it replaces the binary, keeps an existing config and
//! client key, and only rewrites the unit and the rule when they changed.

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::config::{self, Config};
use crate::tv;

const UNIT_FILE: &str = "tv-controller@.service";
const UNIT: &str = include_str!("../deploy/tv-controller@.service");
const RULE_PATH: &str = "/etc/udev/rules.d/90-lgtv-wake.rules";
const RULE: &str = include_str!("../deploy/90-lgtv-wake.rules");

#[derive(Debug, clap::Args)]
pub struct Options {
    /// TV IP address (used only when writing a new config)
    #[arg(long)]
    host: Option<String>,
    /// TV MAC address, e.g. aa:bb:cc:dd:ee:ff (new config only)
    #[arg(long)]
    mac: Option<String>,
    /// Broadcast address for Wake-on-LAN (new config only)
    #[arg(long)]
    broadcast: Option<Ipv4Addr>,
    /// TV input to switch to, e.g. HDMI_1 (new config only)
    #[arg(long)]
    input: Option<String>,
    /// Print the udev commands instead of running them with sudo
    #[arg(long)]
    no_sudo: bool,
    /// Don't pair with the TV
    #[arg(long)]
    no_pair: bool,
}

pub async fn run(opts: &Options) -> Result<()> {
    if !cfg!(target_os = "linux") {
        bail!("setup is only supported on Linux");
    }
    install_binary()?;
    let cfg = ensure_config(opts)?;
    install_unit()?;
    install_rule(opts.no_sudo)?;

    if config::load_client_key()?.is_some() {
        println!("client key: already paired");
    } else if opts.no_pair {
        println!("client key: not paired; run `lgtv-wake pair` with the TV on");
    } else {
        println!("client key: pairing (the TV must be on)");
        if let Err(e) = tv::pair(&cfg, false).await {
            println!("pairing failed: {e:#}\nRun `lgtv-wake pair` later with the TV on.");
        }
    }

    println!(
        "\nDone. Turn a controller on, then follow the logs with:\n  journalctl --user -u 'tv-controller@*' -f"
    );
    Ok(())
}

/// Copy the running binary to `~/.local/bin/lgtv-wake`.
fn install_binary() -> Result<()> {
    let target = dirs::home_dir()
        .context("could not determine the home directory")?
        .join(".local/bin/lgtv-wake");
    let current = std::env::current_exe().context("could not find the running binary")?;
    if fs::canonicalize(&current).ok() == fs::canonicalize(&target).ok() {
        println!("binary: already running from {}", target.display());
        return Ok(());
    }
    let dir = target.parent().expect("target has a parent");
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    // Copy then rename, so a watcher running the old binary isn't disturbed.
    let tmp = target.with_extension("new");
    fs::copy(&current, &tmp).with_context(|| format!("copying to {}", tmp.display()))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
    fs::rename(&tmp, &target).with_context(|| format!("installing {}", target.display()))?;
    println!("binary: installed {}", target.display());
    Ok(())
}

/// Keep an existing config (after checking it parses), or write a new one
/// from the flags, asking for anything missing.
fn ensure_config(opts: &Options) -> Result<Config> {
    let path = config::config_path()?;
    if path.exists() {
        let cfg = Config::load_from(&path)?;
        println!("config: keeping {}", path.display());
        return Ok(cfg);
    }

    let host = value(opts.host.clone(), "host", "TV IP address", None)?;
    let mac = value(opts.mac.clone(), "mac", "TV MAC address", None)?;
    let broadcast = value(
        opts.broadcast.map(|b| b.to_string()),
        "broadcast",
        "Broadcast address",
        default_broadcast(&host).map(|b| b.to_string()),
    )?;
    let input = value(
        opts.input.clone(),
        "input",
        "TV input",
        Some("HDMI_1".into()),
    )?;

    let text = new_config(&host, &mac, &broadcast, &input);
    let cfg = Config::parse(&text)?;
    let dir = path.parent().expect("config path has a parent");
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    println!("config: wrote {}", path.display());
    Ok(cfg)
}

/// A flag's value, or ask for it on the terminal.
fn value(flag: Option<String>, name: &str, label: &str, default: Option<String>) -> Result<String> {
    if let Some(v) = flag {
        return Ok(v);
    }
    if !io::stdin().is_terminal() {
        match default {
            Some(d) => return Ok(d),
            None => bail!("no config yet and no terminal to ask: pass --{name}"),
        }
    }
    loop {
        match &default {
            Some(d) => print!("{label} [{d}]: "),
            None => print!("{label}: "),
        }
        io::stdout().flush()?;
        let mut line = String::new();
        if io::stdin().read_line(&mut line)? == 0 {
            bail!("no value for --{name}");
        }
        match (line.trim(), &default) {
            ("", Some(d)) => return Ok(d.clone()),
            ("", None) => continue,
            (v, _) => return Ok(v.to_string()),
        }
    }
}

/// `a.b.c.255` for an IPv4 host (assumes a /24).
fn default_broadcast(host: &str) -> Option<Ipv4Addr> {
    let [a, b, c, _] = host.parse::<Ipv4Addr>().ok()?.octets();
    Some(Ipv4Addr::new(a, b, c, 255))
}

fn new_config(host: &str, mac: &str, broadcast: &str, input: &str) -> String {
    format!(
        r#"host = "{host}"
mac = "{mac}"
broadcast = "{broadcast}"
input = "{input}"
# optional, with these defaults:
# wake_delay_secs = 5
# long_press_secs = 5
# wake_timeout_secs = 20
# tls = "insecure"   # accept any certificate from `host` (if the pinned one changed)
"#
    )
}

/// Write `~/.config/systemd/user/tv-controller@.service` and reload the user manager.
fn install_unit() -> Result<()> {
    let path = dirs::config_dir()
        .context("could not determine the config directory")?
        .join("systemd/user")
        .join(UNIT_FILE);
    if fs::read_to_string(&path).ok().as_deref() == Some(UNIT) {
        println!("unit: {} is up to date", path.display());
        return Ok(());
    }
    let dir = path.parent().expect("unit path has a parent");
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    fs::write(&path, UNIT).with_context(|| format!("writing {}", path.display()))?;
    println!("unit: wrote {}", path.display());
    run_cmd("systemctl", &["--user", "daemon-reload"])
}

/// Install the udev rule with sudo (or print the commands with `--no-sudo`).
fn install_rule(no_sudo: bool) -> Result<()> {
    if fs::read_to_string(RULE_PATH).ok().as_deref() == Some(RULE) {
        println!("udev rule: {RULE_PATH} is up to date");
        return Ok(());
    }
    let tmp = staged_rule_path();
    fs::write(&tmp, RULE).with_context(|| format!("writing {}", tmp.display()))?;
    let tmp = tmp.to_str().context("temp path is not UTF-8")?;
    let install = ["install", "-m", "0644", tmp, RULE_PATH];
    let reload = ["udevadm", "control", "--reload"];

    if no_sudo {
        println!(
            "udev rule: run these to install it:\n  sudo {}\n  sudo {}",
            install.join(" "),
            reload.join(" ")
        );
        return Ok(());
    }
    println!("udev rule: installing {RULE_PATH} (sudo may ask for your password)");
    run_cmd("sudo", &install)?;
    run_cmd("sudo", &reload)?;
    let _ = fs::remove_file(tmp);
    Ok(())
}

fn staged_rule_path() -> PathBuf {
    std::env::temp_dir().join("90-lgtv-wake.rules")
}

fn run_cmd(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("running {program}"))?;
    if !status.success() {
        bail!("`{program} {}` failed ({status})", args.join(" "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_from_host() {
        assert_eq!(
            default_broadcast("192.168.1.50"),
            Some(Ipv4Addr::new(192, 168, 1, 255))
        );
        assert_eq!(default_broadcast("tv.local"), None);
    }

    #[test]
    fn new_config_parses() {
        let cfg = Config::parse(&new_config(
            "192.168.1.50",
            "aa:bb:cc:dd:ee:ff",
            "192.168.1.255",
            "HDMI_1",
        ))
        .unwrap();
        assert_eq!(cfg.host, "192.168.1.50");
        assert_eq!(cfg.input, "HDMI_1");
        assert_eq!(cfg.wake_delay_secs, 5);
    }

    #[test]
    fn embedded_files() {
        assert!(UNIT.contains("ExecStart=%h/.local/bin/lgtv-wake watch /dev/input/%i"));
        assert!(RULE.contains("SYSTEMD_USER_WANTS}+=\"tv-controller@%k.service\""));
    }
}
