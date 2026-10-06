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
        "\nDone. Turn a controller on, then follow the logs with:\n  journalctl -t lgtv-wake -f"
    );
    Ok(())
}

/// Copy the running binary to `~/.local/bin/lgtv-wake`.
fn install_binary() -> Result<()> {
    let target = installed_path()?;
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

/// Where `setup` installs the binary: `~/.local/bin/lgtv-wake`.
pub fn installed_path() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("could not determine the home directory")?
        .join(".local/bin/lgtv-wake"))
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

/// The broadcast address of the local subnet containing `host`, from
/// `/proc/net/route`; `a.b.c.255` (a /24) if no route matches.
fn default_broadcast(host: &str) -> Option<Ipv4Addr> {
    let host = host.parse::<Ipv4Addr>().ok()?;
    let routes = fs::read_to_string("/proc/net/route").unwrap_or_default();
    broadcast_from_routes(host, &routes).or_else(|| {
        let [a, b, c, _] = host.octets();
        Some(Ipv4Addr::new(a, b, c, 255))
    })
}

/// Find the most specific on-link route (no gateway) in `/proc/net/route`
/// text that contains `host`, and return its subnet's broadcast address.
fn broadcast_from_routes(host: Ipv4Addr, routes: &str) -> Option<Ipv4Addr> {
    // Addresses are hex of the network-order bytes read as a native u32.
    let addr = |hex: &str| {
        u32::from_str_radix(hex, 16)
            .ok()
            .map(|n| u32::from_be_bytes(n.to_ne_bytes()))
    };
    let host = u32::from(host);
    routes
        .lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            let (dest, gateway, mask) = (addr(f.get(1)?)?, addr(f.get(2)?)?, addr(f.get(7)?)?);
            (gateway == 0 && mask != 0 && host & mask == dest & mask).then_some((dest, mask))
        })
        .max_by_key(|&(_, mask)| mask.count_ones())
        .map(|(dest, mask)| Ipv4Addr::from(dest & mask | !mask))
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

pub fn run_cmd(program: &str, args: &[&str]) -> Result<()> {
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

    // `/proc/net/route` from a /22 network, written for a little-endian host.
    const ROUTES: &str = "\
Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
enp1s0\t00000000\t0108A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0
enp1s0\t0008A8C0\t00000000\t0001\t0\t0\t100\t00FCFFFF\t0\t0\t0
docker0\t000011AC\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0
";

    #[test]
    #[cfg(target_endian = "little")]
    fn broadcast_from_route_table() {
        let host = |s: &str| s.parse::<Ipv4Addr>().unwrap();
        assert_eq!(
            broadcast_from_routes(host("192.168.8.5"), ROUTES),
            Some(Ipv4Addr::new(192, 168, 11, 255))
        );
        assert_eq!(
            broadcast_from_routes(host("172.17.0.9"), ROUTES),
            Some(Ipv4Addr::new(172, 17, 255, 255))
        );
        // Only reachable through the default route: no on-link subnet.
        assert_eq!(broadcast_from_routes(host("10.0.0.5"), ROUTES), None);
    }

    #[test]
    fn broadcast_fallback() {
        assert_eq!(
            default_broadcast("198.51.100.50"),
            Some(Ipv4Addr::new(198, 51, 100, 255))
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
