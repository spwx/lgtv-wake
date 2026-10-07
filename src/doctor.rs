//! `doctor`: check the install and the TV connection, one line per item with a hint for
//! anything wrong. Exits non-zero if any check fails; warnings (e.g. the TV being off)
//! don't count.

use std::fmt::Display;
use std::fs;
use std::time::Duration;

use anyhow::Result;

use crate::config::{self, Config, TlsMode};
use crate::ssap::{self, Client};
use crate::{cert, pin, tls, tv};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REGISTER_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Ok,
    Warn,
    Fail,
}

/// Prints each check as it runs and counts the failures.
#[derive(Debug, Default)]
struct Report {
    failures: usize,
}

impl Report {
    fn ok(&mut self, item: &str, detail: impl Display) {
        self.line(Level::Ok, item, detail, None);
    }

    fn warn(&mut self, item: &str, detail: impl Display, hint: &str) {
        self.line(Level::Warn, item, detail, Some(hint));
    }

    fn fail(&mut self, item: &str, detail: impl Display, hint: &str) {
        self.line(Level::Fail, item, detail, Some(hint));
    }

    fn line(&mut self, level: Level, item: &str, detail: impl Display, hint: Option<&str>) {
        println!("{}", format_line(level, item, &detail.to_string(), hint));
        if level == Level::Fail {
            self.failures += 1;
        }
    }
}

fn format_line(level: Level, item: &str, detail: &str, hint: Option<&str>) -> String {
    let tag = match level {
        Level::Ok => "ok",
        Level::Warn => "warn",
        Level::Fail => "FAIL",
    };
    let mut line = format!("{tag:<5} {item}: {detail}");
    if let Some(hint) = hint.filter(|h| !h.is_empty()) {
        line.push_str(&format!("\n      → {hint}"));
    }
    line
}

pub async fn run() -> Result<()> {
    let mut report = Report::default();

    let cfg = check_config(&mut report);
    let key = check_key(&mut report);
    #[cfg(target_os = "linux")]
    linux::check(&mut report);
    if let Some(cfg) = &cfg {
        check_tv(&mut report, cfg, key.as_deref()).await;
    }

    if report.failures > 0 {
        println!(
            "\n{} problem{} found",
            report.failures,
            if report.failures == 1 { "" } else { "s" }
        );
        return Err(tv::Reported.into());
    }
    println!("\nAll checks passed");
    Ok(())
}

fn check_config(report: &mut Report) -> Option<Config> {
    let path = match config::config_path() {
        Ok(path) => path,
        Err(e) => {
            report.fail("config", format!("{e:#}"), "");
            return None;
        }
    };
    match Config::load_from(&path) {
        Ok(cfg) => {
            report.ok(
                "config",
                format!(
                    "{} (host {}, input {})",
                    path.display(),
                    cfg.host,
                    cfg.input
                ),
            );
            Some(cfg)
        }
        Err(e) => {
            // The first line only: a missing file's error includes the whole template.
            let first = format!("{e:#}");
            let first = first.lines().next().unwrap_or_default().to_owned();
            report.fail(
                "config",
                first,
                "run `lgtv-wake setup` (Linux); `lgtv-wake status` prints a template",
            );
            None
        }
    }
}

/// The client key, if present (even with the wrong permissions).
fn check_key(report: &mut Report) -> Option<String> {
    let path = match config::key_path() {
        Ok(path) => path,
        Err(e) => {
            report.fail("client key", format!("{e:#}"), "");
            return None;
        }
    };
    let key = match config::load_client_key_from(&path) {
        Ok(Some(key)) => key,
        Ok(None) => {
            report.fail(
                "client key",
                format!("missing ({})", path.display()),
                "run `lgtv-wake pair` with the TV on, and accept the prompt",
            );
            return None;
        }
        Err(e) => {
            report.fail("client key", format!("{e:#}"), "");
            return None;
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match fs::metadata(&path).map(|m| m.permissions().mode() & 0o777) {
            Ok(0o600) => report.ok("client key", format!("{} (mode 0600)", path.display())),
            Ok(mode) => report.fail(
                "client key",
                format!("{} has mode {mode:04o}, expected 0600", path.display()),
                &format!("chmod 600 {}", path.display()),
            ),
            Err(e) => report.fail("client key", format!("{}: {e}", path.display()), ""),
        }
    }
    #[cfg(not(unix))]
    report.ok("client key", path.display());
    Some(key)
}

/// Whether the TV answers, presents the pinned certificate and accepts the client key.
async fn check_tv(report: &mut Report, cfg: &Config, key: Option<&str>) {
    let target = format!("{}:{}", cfg.host, ssap::PORT);
    let der = match pin::fetch(&cfg.host).await {
        Ok(der) => der,
        Err(e) if ssap::is_unreachable(&e) => {
            report.warn(
                "TV",
                format!("off/standby (no answer at {target})"),
                "turn the TV on to check its certificate and the client key",
            );
            return;
        }
        Err(e) => {
            report.fail("TV", format!("{e:#}"), "check `host` in the config");
            return;
        }
    };
    report.ok("TV", format!("answering at {target}"));

    let pinned = match cfg.tls {
        TlsMode::Insecure => {
            report.warn(
                "certificate",
                "not checked (`tls = \"insecure\"`)",
                "run `lgtv-wake pin`, then remove `tls = \"insecure\"` from the config",
            );
            true
        }
        TlsMode::Pinned => match tls::current_pin() {
            Ok(pin) if pin.der == der => {
                report.ok("certificate", format!("matches the pin ({})", pin.source()));
                true
            }
            Ok(pin) => {
                report.fail(
                    "certificate",
                    format!(
                        "the TV's ({}) doesn't match the pin ({}, {})",
                        cert::fingerprint(&der),
                        cert::fingerprint(&pin.der),
                        pin.source()
                    ),
                    "if the TV's firmware was updated, run `lgtv-wake pin`",
                );
                false
            }
            Err(e) => {
                report.fail("certificate", format!("{e:#}"), "");
                false
            }
        },
    };

    if let (true, Some(key)) = (pinned, key) {
        check_pairing(report, cfg, key).await;
    }
}

async fn check_pairing(report: &mut Report, cfg: &Config, key: &str) {
    let result = async {
        let mut client = Client::connect(cfg, CONNECT_TIMEOUT).await?;
        client.register(Some(key), REGISTER_TIMEOUT).await?;
        client.close().await;
        anyhow::Ok(())
    }
    .await;
    match result {
        Ok(()) => report.ok("pairing", "the TV accepted the client key"),
        Err(e) if ssap::is_busy(&e) || ssap::is_unreachable(&e) => report.warn(
            "pairing",
            format!("not checked, the TV is turning off ({e:#})"),
            "run `lgtv-wake doctor` again once it's on",
        ),
        Err(e) => report.fail(
            "pairing",
            format!("{e:#}"),
            "run `lgtv-wake pair --force` with the TV on, and accept the prompt",
        ),
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::process::Command;

    use super::*;
    use crate::setup;

    pub fn check(report: &mut Report) {
        check_binary(report);
        check_file(report, "udev rule", setup::RULE_PATH.as_ref(), setup::RULE);
        match setup::unit_path() {
            Ok(path) => check_file(report, "user unit", &path, setup::UNIT),
            Err(e) => report.fail("user unit", format!("{e:#}"), ""),
        }
        check_watchers(report);
    }

    /// `~/.local/bin/lgtv-wake` exists and is this version.
    fn check_binary(report: &mut Report) {
        let path = match setup::installed_path() {
            Ok(path) => path,
            Err(e) => {
                report.fail("binary", format!("{e:#}"), "");
                return;
            }
        };
        if !path.exists() {
            report.fail(
                "binary",
                format!("{} is missing", path.display()),
                "run `lgtv-wake setup`",
            );
            return;
        }
        let ours = concat!("lgtv-wake ", env!("CARGO_PKG_VERSION"));
        match Command::new(&path).arg("--version").output() {
            Ok(out) if out.status.success() => {
                let theirs = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                if theirs == ours {
                    report.ok("binary", format!("{} ({theirs})", path.display()));
                } else {
                    report.warn(
                        "binary",
                        format!("{} is {theirs}, this is {ours}", path.display()),
                        "run `lgtv-wake update` (or `setup` from the newer binary)",
                    );
                }
            }
            Ok(out) => report.fail(
                "binary",
                format!("{} --version failed ({})", path.display(), out.status),
                "run `lgtv-wake setup`",
            ),
            Err(e) => report.fail(
                "binary",
                format!("running {}: {e}", path.display()),
                "run `lgtv-wake setup`",
            ),
        }
    }

    /// An installed file matches the copy embedded in this binary.
    fn check_file(report: &mut Report, item: &str, path: &std::path::Path, embedded: &str) {
        match fs::read_to_string(path) {
            Ok(text) if text == embedded => report.ok(item, path.display()),
            Ok(_) => report.fail(
                item,
                format!("{} differs from this version's", path.display()),
                "run `lgtv-wake setup`",
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => report.fail(
                item,
                format!("{} is missing", path.display()),
                "run `lgtv-wake setup`",
            ),
            Err(e) => report.fail(item, format!("{}: {e}", path.display()), ""),
        }
    }

    /// The running `tv-controller@` units (information only: none is normal with no
    /// controller, keyboard or mouse connected).
    fn check_watchers(report: &mut Report) {
        let out = Command::new("systemctl")
            .args(["--user", "list-units", "tv-controller@*"])
            .args(["--state=active", "--plain", "--no-legend"])
            .output();
        match out {
            Ok(out) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let units = watcher_units(&stdout);
                if units.is_empty() {
                    report.ok(
                        "watchers",
                        "none running (no controller, keyboard or mouse)",
                    );
                } else {
                    report.ok("watchers", format!("running: {}", units.join(" ")));
                }
            }
            Ok(out) => report.warn(
                "watchers",
                format!("`systemctl --user list-units` failed ({})", out.status),
                "is the systemd user manager running?",
            ),
            Err(e) => report.warn("watchers", format!("running systemctl: {e}"), ""),
        }
    }
}

/// Unit names from `systemctl list-units --plain --no-legend` output.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn watcher_units(list_units: &str) -> Vec<&str> {
    list_units
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|unit| unit.starts_with("tv-controller@"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_format() {
        assert_eq!(
            format_line(Level::Ok, "config", "fine", None),
            "ok    config: fine"
        );
        assert_eq!(
            format_line(Level::Fail, "client key", "missing", Some("run pair")),
            "FAIL  client key: missing\n      → run pair"
        );
        assert_eq!(
            format_line(Level::Warn, "TV", "off", Some("")),
            "warn  TV: off"
        );
    }

    #[test]
    fn counts_failures_only() {
        let mut report = Report::default();
        report.ok("a", "fine");
        report.warn("b", "meh", "");
        report.fail("c", "bad", "fix it");
        assert_eq!(report.failures, 1);
    }

    #[test]
    fn parses_watcher_units() {
        let out = "tv-controller@event17.service loaded active running LG TV wake for input device event17\n\
                   tv-controller@event3.service  loaded active running LG TV wake for input device event3\n";
        assert_eq!(
            watcher_units(out),
            [
                "tv-controller@event17.service",
                "tv-controller@event3.service"
            ]
        );
        assert!(watcher_units("").is_empty());
    }
}
