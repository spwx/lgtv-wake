//! `setup`: install the binary, config, systemd user unit, udev rule and the sleep and
//! power-off hooks (Linux).
//!
//! Safe to run again: it replaces the binary, keeps an existing config and
//! client key, and only rewrites the units, rule and hooks when they changed.

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::config::{self, Config};
use crate::tv;
use crate::watch::is_controller;

const UNIT_FILE: &str = "tv-controller@.service";
pub const UNIT: &str = include_str!("../deploy/tv-controller@.service");
pub const RULE_PATH: &str = "/etc/udev/rules.d/90-lgtv-wake.rules";
pub const RULE: &str = include_str!("../deploy/90-lgtv-wake.rules");
pub const SHUTDOWN_UNIT_PATH: &str = "/etc/systemd/system/lgtv-wake-shutdown@.service";
pub const SHUTDOWN_UNIT: &str = include_str!("../deploy/lgtv-wake-shutdown@.service");
pub const DISPATCHER_DIR: &str = "/etc/NetworkManager/dispatcher.d";
pub const DISPATCHER_PATH: &str = "/etc/NetworkManager/dispatcher.d/pre-down.d/90-lgtv-wake";
/// With `@USER@` for the user to run `lgtv-wake` as.
const DISPATCHER: &str = include_str!("../deploy/90-lgtv-wake.dispatcher");

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
    /// Print the commands that need root instead of running them with sudo
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
    restart_watchers();
    install_rule(opts.no_sudo)?;
    install_power_hooks(opts.no_sudo)?;

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
        "\nDone. Turn a controller on or press a key, then follow the logs with:\n  journalctl -t lgtv-wake -f"
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
# long_press_secs = 3
# wake_timeout_secs = 20
# idle_off_mins = 15   # Game Mode only; 0 disables
# off_grace_secs = 30  # ignore keyboard and mouse this long after turning the TV off
# tls = "insecure"   # accept any certificate from `host` (if the pinned one changed)
"#
    )
}

/// Write `~/.config/systemd/user/tv-controller@.service` and reload the user manager.
fn install_unit() -> Result<()> {
    let path = unit_path()?;
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

/// `~/.config/systemd/user/tv-controller@.service`.
pub fn unit_path() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("could not determine the config directory")?
        .join("systemd/user")
        .join(UNIT_FILE))
}

/// Restart the running keyboard and mouse watchers so they run the new binary.
///
/// Controller watchers are left alone: starting one wakes the TV and switches its input.
/// Failures only warn, since the old watchers keep working.
fn restart_watchers() {
    let output = Command::new("systemctl")
        .args(["--user", "list-units", "tv-controller@*"])
        .args(["--state=active", "--plain", "--no-legend"])
        .output();
    let units = match output {
        Ok(o) if o.status.success() => units_to_restart(
            &String::from_utf8_lossy(&o.stdout),
            Path::new("/sys/class/input"),
        ),
        Ok(o) => {
            println!("watchers: could not list them ({})", o.status);
            return;
        }
        Err(e) => {
            println!("watchers: could not list them ({e})");
            return;
        }
    };
    if units.is_empty() {
        return;
    }
    let mut args = vec!["--user", "restart"];
    args.extend(units.iter().map(String::as_str));
    match run_cmd("systemctl", &args) {
        Ok(()) => println!("watchers: restarted {}", units.join(" ")),
        Err(e) => println!("watchers: restart failed: {e:#}"),
    }
}

/// The `tv-controller@eventN.service` units in `systemctl list-units --plain --no-legend`
/// output whose device, looked up under `class_input` (normally `/sys/class/input`), isn't
/// the controller. Units whose device name can't be read are skipped.
fn units_to_restart(list_units: &str, class_input: &Path) -> Vec<String> {
    list_units
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|unit| {
            let Some(event) = unit
                .strip_prefix("tv-controller@")
                .and_then(|u| u.strip_suffix(".service"))
            else {
                return false;
            };
            if !event.starts_with("event") || event.contains('/') {
                return false;
            }
            fs::read_to_string(class_input.join(event).join("device/name"))
                .is_ok_and(|name| !is_controller(&name))
        })
        .map(String::from)
        .collect()
}

/// Install the udev rule with sudo (or print the commands with `--no-sudo`).
fn install_rule(no_sudo: bool) -> Result<()> {
    if fs::read_to_string(RULE_PATH).ok().as_deref() == Some(RULE) {
        println!("udev rule: {RULE_PATH} is up to date");
        return Ok(());
    }
    let tmp = stage("90-lgtv-wake.rules", RULE)?;
    let cmds = [
        cmd(&["install", "-m", "0644", &tmp, RULE_PATH]),
        cmd(&["udevadm", "control", "--reload"]),
        // Apply the rule to keyboards and mice already connected; controllers start on connect.
        cmd(&[
            "udevadm",
            "trigger",
            "--action=change",
            "--subsystem-match=input",
            "--property-match=ID_INPUT_KEYBOARD=1",
            "--property-match=ID_INPUT_MOUSE=1",
        ]),
    ];
    as_root("udev rule", RULE_PATH, no_sudo, &cmds, &[tmp])
}

/// Install the power-off unit and the sleep hook, which turn the TV off when the system
/// powers off or sleeps, with sudo (or print the commands with `--no-sudo`).
fn install_power_hooks(no_sudo: bool) -> Result<()> {
    let user = current_user()?;
    let instance = shutdown_instance(&user);
    let script = dispatcher_script(&user);
    let unit_ok = fs::read_to_string(SHUTDOWN_UNIT_PATH).ok().as_deref() == Some(SHUTDOWN_UNIT);
    // Without NetworkManager there's no pre-down hook, and sleep leaves the TV alone.
    let nm = Path::new(DISPATCHER_DIR).is_dir();
    let script_ok = !nm || fs::read_to_string(DISPATCHER_PATH).ok().as_deref() == Some(&script);
    let enabled = ["is-enabled", "is-active"].iter().all(|check| {
        Command::new("systemctl")
            .args([check, "--quiet", instance.as_str()])
            .status()
            .is_ok_and(|s| s.success())
    });
    if !nm {
        println!("power hooks: no NetworkManager, so sleep leaves the TV on");
    }
    if unit_ok && script_ok && enabled {
        println!("power hooks: {instance} and the sleep hook are up to date");
        return Ok(());
    }

    let mut staged = Vec::new();
    let mut cmds = Vec::new();
    if !unit_ok {
        let tmp = stage("lgtv-wake-shutdown@.service", SHUTDOWN_UNIT)?;
        cmds.push(cmd(&["install", "-m", "0644", &tmp, SHUTDOWN_UNIT_PATH]));
        cmds.push(cmd(&["systemctl", "daemon-reload"]));
        staged.push(tmp);
    }
    if !script_ok {
        let tmp = stage("90-lgtv-wake.dispatcher", &script)?;
        // `-D` creates pre-down.d if it's missing.
        cmds.push(cmd(&["install", "-D", "-m", "0755", &tmp, DISPATCHER_PATH]));
        staged.push(tmp);
    }
    cmds.push(cmd(&["systemctl", "enable", "--now", &instance]));
    as_root("power hooks", &instance, no_sudo, &cmds, &staged)
}

/// The user running `setup`, whom the power hooks run `lgtv-wake` as.
pub fn current_user() -> Result<String> {
    let out = Command::new("id")
        .arg("-un")
        .output()
        .context("running id -un")?;
    let user = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if !out.status.success() || !valid_user(&user) {
        bail!("could not determine the user name (`id -un` gave {user:?})");
    }
    Ok(user)
}

/// A user name that's safe in a unit instance name and in a shell script.
fn valid_user(user: &str) -> bool {
    !user.is_empty()
        && !user.starts_with('-')
        && user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

pub fn shutdown_instance(user: &str) -> String {
    format!("lgtv-wake-shutdown@{user}.service")
}

pub fn dispatcher_script(user: &str) -> String {
    DISPATCHER.replace("@USER@", user)
}

/// Write `content` to a temp file for `sudo install` to copy into place.
fn stage(name: &str, content: &str) -> Result<String> {
    let path = std::env::temp_dir().join(name);
    fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
    Ok(path.to_str().context("temp path is not UTF-8")?.to_owned())
}

fn cmd(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

/// Run `cmds` with sudo and remove the `staged` files, or with `no_sudo` print the commands
/// (keeping the files they install from).
fn as_root(
    label: &str,
    what: &str,
    no_sudo: bool,
    cmds: &[Vec<String>],
    staged: &[String],
) -> Result<()> {
    if no_sudo {
        println!("{label}: run these to install {what}:");
        for c in cmds {
            println!("  sudo {}", c.join(" "));
        }
        return Ok(());
    }
    println!("{label}: installing {what} (sudo may ask for your password)");
    for c in cmds {
        let args: Vec<&str> = c.iter().map(String::as_str).collect();
        run_cmd("sudo", &args)?;
    }
    for tmp in staged {
        let _ = fs::remove_file(tmp);
    }
    Ok(())
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
    use crate::watch::CONTROLLER_NAMES;

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
    fn restarts_keyboards_and_mice_only() {
        let t = TempDir::new();
        t.device("event3", "AT Translated Set 2 keyboard");
        t.device("event7", "Logitech USB Receiver Mouse");
        t.device("event17", CONTROLLER_NAMES[0]);
        t.device("event15", CONTROLLER_NAMES[1]);
        // An eventN without a readable name (removed meanwhile) is skipped.
        fs::create_dir_all(t.0.join("event30")).unwrap();
        let list = "\
tv-controller@event3.service  loaded active running Watch event3 for the TV
tv-controller@event15.service loaded active running Watch event15 for the TV
tv-controller@event17.service loaded active running Watch event17 for the TV
tv-controller@event30.service loaded active running Watch event30 for the TV
tv-controller@event7.service  loaded active running Watch event7 for the TV
tv-controller@event99.service loaded active running Watch event99 for the TV
";
        assert_eq!(
            units_to_restart(list, &t.0),
            [
                "tv-controller@event3.service",
                "tv-controller@event7.service"
            ]
        );
        assert!(units_to_restart("", &t.0).is_empty());
    }

    /// Temp dir that removes itself on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir()
                .join(format!("lgtv-wake-setup-{}-{nanos}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        /// Add `<entry>/device/name` containing `name` plus a newline, like sysfs.
        fn device(&self, entry: &str, name: &str) {
            let dir = self.0.join(entry).join("device");
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("name"), format!("{name}\n")).unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn embedded_files() {
        assert!(UNIT.contains("ExecStart=%h/.local/bin/lgtv-wake watch /dev/input/%i"));
        assert!(RULE.contains("SYSTEMD_USER_WANTS}+=\"tv-controller@%k.service\""));
        assert!(RULE.contains("LABEL=\"lgtv_wake_desk\""));
        assert!(RULE.contains("ATTRS{name}==\"QEMU*\", GOTO=\"lgtv_wake_end\""));
        assert!(SHUTDOWN_UNIT.contains("User=%i"));
        assert!(SHUTDOWN_UNIT.contains("/.local/bin/lgtv-wake\" system-off'"));
        assert!(SHUTDOWN_UNIT.contains("WantedBy=multi-user.target"));
        assert!(DISPATCHER.starts_with("#!/bin/sh\n"));
    }

    #[test]
    fn power_hooks_for_user() {
        assert_eq!(shutdown_instance("spw"), "lgtv-wake-shutdown@spw.service");
        let script = dispatcher_script("spw");
        assert!(script.contains("\nuser=spw\n"));
        assert!(!script.contains("@USER@"));
        assert!(script.contains("lgtv-wake\" system-off"));
    }

    #[test]
    fn user_names() {
        assert!(valid_user("spw"));
        assert!(valid_user("first.last-2_x"));
        assert!(!valid_user(""));
        assert!(!valid_user("-rf"));
        assert!(!valid_user("a b"));
        assert!(!valid_user("a$(x)"));
    }
}
