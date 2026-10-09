//! `system-off`: disconnect the Xbox controllers over Bluetooth (which powers them off) and
//! turn the TV off when the system sleeps or powers off, but not when it reboots. Run by the
//! hooks `setup` installs: a NetworkManager pre-down script (sleep) and the
//! `lgtv-wake-shutdown@.service` system unit (power-off).

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::process::Command;
use tokio::task::JoinSet;
use tracing::{info, warn};

use crate::config::Config;
use crate::tv;
use crate::watch::{SYSFS_INPUT, controller_macs_in};

/// Give up on the TV and on each disconnect after this long, so sleep or shutdown is never
/// held up for long. A TV that's on answers well within it; one in standby doesn't answer at
/// all.
const LIMIT: Duration = Duration::from_secs(3);

/// Targets whose start job means the system is restarting rather than powering off.
const REBOOT_TARGETS: [&str; 3] = ["reboot.target", "kexec.target", "soft-reboot.target"];

pub async fn run(cfg: &Config) -> Result<()> {
    if let Some(target) = reboot_job().await {
        info!("system is rebooting ({target}), leaving the TV on");
        return Ok(());
    }
    // Both at once, so together they still take at most `LIMIT`.
    let (tv, ()) = tokio::join!(tv_off(cfg), disconnect_controllers());
    tv
}

async fn tv_off(cfg: &Config) -> Result<()> {
    match tokio::time::timeout(LIMIT, tv::off(cfg)).await {
        Ok(res) => res,
        Err(_) => bail!(
            "no answer from the TV within {}s, leaving it",
            LIMIT.as_secs()
        ),
    }
}

/// Disconnect every connected controller, logging (not returning) failures, so they never
/// stop the TV from turning off.
async fn disconnect_controllers() {
    let macs = match controller_macs_in(Path::new(SYSFS_INPUT)) {
        Ok(macs) => macs,
        Err(e) => {
            warn!("listing controllers in {SYSFS_INPUT}: {e}");
            return;
        }
    };
    let mut tasks = JoinSet::new();
    for mac in macs {
        tasks.spawn(async move {
            let res = disconnect(&mac).await;
            (mac, res)
        });
    }
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok((mac, Ok(()))) => info!("disconnected controller {mac}"),
            Ok((mac, Err(e))) => warn!("disconnecting controller {mac}: {e:#}"),
            Err(e) => warn!("disconnecting a controller: {e}"),
        }
    }
}

/// Disconnect one controller with `bluetoothctl`, within `LIMIT`.
async fn disconnect(mac: &str) -> Result<()> {
    let out = tokio::time::timeout(
        LIMIT,
        Command::new("bluetoothctl")
            .args(["disconnect", mac])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("bluetoothctl timed out")?
    .context("running bluetoothctl")?;
    anyhow::ensure!(
        out.status.success(),
        "bluetoothctl failed ({}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

/// The reboot target systemd is starting, if any. When it can't tell, assume a power-off.
async fn reboot_job() -> Option<String> {
    let out = Command::new("systemctl")
        .args(["list-jobs", "--no-legend", "--no-pager"])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => reboot_target(&String::from_utf8_lossy(&o.stdout)),
        Ok(o) => {
            warn!(
                "systemctl list-jobs failed ({}), assuming no reboot",
                o.status
            );
            None
        }
        Err(e) => {
            warn!("running systemctl list-jobs: {e}, assuming no reboot");
            None
        }
    }
}

/// Find a reboot target's start job in `systemctl list-jobs --no-legend` output
/// (`JOB UNIT TYPE STATE` per line).
fn reboot_target(list_jobs: &str) -> Option<String> {
    list_jobs.lines().find_map(|line| {
        let f: Vec<&str> = line.split_whitespace().collect();
        match f.as_slice() {
            [_, unit, "start", ..] if REBOOT_TARGETS.contains(unit) => Some(unit.to_string()),
            _ => None,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_reboot_jobs() {
        let reboot = "\
 2861 reboot.target                start waiting
 2900 systemd-reboot.service       start waiting
 2875 shutdown.target              start waiting
";
        assert_eq!(reboot_target(reboot).as_deref(), Some("reboot.target"));
        let soft = " 12 soft-reboot.target start waiting\n";
        assert_eq!(reboot_target(soft).as_deref(), Some("soft-reboot.target"));
    }

    #[test]
    fn power_off_and_sleep_are_not_reboots() {
        let poweroff = "\
 2861 poweroff.target              start waiting
 2875 shutdown.target              start waiting
";
        assert_eq!(reboot_target(poweroff), None);
        assert_eq!(reboot_target(""), None);
        assert_eq!(reboot_target(" 3 reboot.target stop waiting\n"), None);
    }
}
