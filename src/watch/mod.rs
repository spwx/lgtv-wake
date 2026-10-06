//! `watch <device>`: per-device evdev loop (Linux only). A controller drives `machine`; a
//! keyboard or mouse turns the TV on with a key press or click.

pub mod machine;

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::ops::RangeInclusive;
use std::path::Path;

/// Device name of the real controller (Steam's virtual pad has a different name).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const CONTROLLER_NAME: &str = "Xbox Wireless Controller";

/// Key codes of touch and tool contacts (`BTN_TOOL_PEN..=BTN_TOOL_QUADTAP`, including
/// `BTN_TOUCH`), sent by touchpads, tablets and touchscreens on contact rather than a click.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const DIGITIZER_KEYS: RangeInclusive<u16> = 0x140..=0x14f;

/// Whether a keyboard or mouse key event turns the TV on: a press (not a release or
/// autorepeat) of a key or button other than a touch contact.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn is_press(code: u16, value: i32) -> bool {
    value == 1 && !DIGITIZER_KEYS.contains(&code)
}

/// Count `eventN` entries under `class_input` (normally `/sys/class/input`) whose
/// `device/name` is [`CONTROLLER_NAME`], skipping the one named `own`.
///
/// Entries whose name can't be read (e.g. removed while we look) are skipped.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn count_controllers_in(class_input: &Path, own: Option<&OsStr>) -> io::Result<usize> {
    let mut count = 0;
    for entry in fs::read_dir(class_input)? {
        let entry = entry?;
        let file_name = entry.file_name();
        if !file_name.to_string_lossy().starts_with("event") || Some(file_name.as_os_str()) == own {
            continue;
        }
        let Ok(name) = fs::read_to_string(entry.path().join("device/name")) else {
            continue;
        };
        if name.trim_end_matches(['\n', '\r']) == CONTROLLER_NAME {
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::OsString;
    use std::future::Future;
    use std::io;
    use std::path::Path;
    use std::pin::Pin;
    use std::process::Output;
    use std::time::Duration;

    use anyhow::{Context, Result};
    use evdev::{Device, EventSummary, InputEvent, KeyCode};
    use tokio::process::Command;
    use tokio::time::{Instant, MissedTickBehavior, interval, sleep, sleep_until, timeout};
    use tracing::{error, info, warn};

    use super::machine::{Action, Event, Machine};
    use super::{CONTROLLER_NAME, count_controllers_in, is_press};
    use crate::config::Config;
    use crate::tv;

    /// `ENODEV`: the device was removed (controller disconnected).
    const ENODEV: i32 = 19;

    /// Tick period while the machine waits for the wake delay.
    const TICK: Duration = Duration::from_millis(250);

    /// How long to wait for access to the device when opening it.
    const OPEN_RETRY: Duration = Duration::from_secs(30);

    /// Minimum time between TV wakes from one keyboard or mouse.
    const DESK_COOLDOWN: Duration = Duration::from_secs(30);

    const SYSFS_INPUT: &str = "/sys/class/input";

    /// logind's `Desktop` for the Steam Game Mode session.
    const GAME_MODE_DESKTOP: &str = "gamescope";

    /// Limit for each `loginctl`/`bluetoothctl` call.
    const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

    type TvFuture<'a> = Pin<Box<dyn Future<Output = Result<()>> + 'a>>;

    /// What one turn of the `select!` produced.
    enum Step {
        Event(Event),
        /// A SYN event: carries no input of its own, not fed to the machine.
        Skip,
        /// The in-flight `tv::on` finished.
        WakeDone(Result<()>),
        /// Read error other than `ENODEV`.
        Fatal(io::Error),
    }

    /// Watch one input device until it disappears, acting on the TV: the Xbox controller with
    /// [`controller`], anything else (a keyboard or mouse, per the udev rule) with [`desk`].
    pub async fn run(cfg: &Config, device: &Path) -> Result<()> {
        let path = device.display().to_string();
        let dev = open(device, &path).await?;
        let name = dev.name().unwrap_or_default().to_owned();
        if name == CONTROLLER_NAME {
            controller(cfg, device, dev, &name, &path).await
        } else {
            desk(cfg, dev, &name, &path).await
        }
    }

    /// Open the device, waiting up to [`OPEN_RETRY`] for access: the user's ACL on it may
    /// only be added once the login session is active, after the unit started.
    async fn open(device: &Path, path: &str) -> Result<Device> {
        let start = Instant::now();
        let mut logged = false;
        loop {
            match Device::open(device) {
                Err(e)
                    if e.kind() == io::ErrorKind::PermissionDenied
                        && start.elapsed() < OPEN_RETRY =>
                {
                    if !logged {
                        info!("{path}: no access yet, retrying for up to {OPEN_RETRY:?}");
                        logged = true;
                    }
                    sleep(Duration::from_secs(1)).await;
                }
                res => return res.with_context(|| format!("opening {path}")),
            }
        }
    }

    /// Keyboard or mouse: a key press or click turns the TV on and switches to `input`, at most
    /// once per [`DESK_COOLDOWN`], so typing doesn't contact the TV on every key.
    async fn desk(cfg: &Config, dev: Device, name: &str, path: &str) -> Result<()> {
        let mut events = dev
            .into_event_stream()
            .with_context(|| format!("reading {path}"))?;
        let mut wake: Option<TvFuture<'_>> = None;
        let mut last_wake: Option<Instant> = None;

        info!("{name} ({path}): watching for key presses and clicks");

        loop {
            tokio::select! {
                res = events.next_event() => match res {
                    Ok(ev) => {
                        if wake.is_none()
                            && is_key_press(ev)
                            && last_wake.is_none_or(|t| t.elapsed() >= DESK_COOLDOWN)
                        {
                            info!("{name} ({path}): key press, turning TV on");
                            last_wake = Some(Instant::now());
                            wake = Some(Box::pin(tv::on(cfg)));
                        }
                    }
                    Err(e) if e.raw_os_error() == Some(ENODEV) => {
                        info!("{name} ({path}): gone");
                        break;
                    }
                    Err(e) => {
                        error!("{name} ({path}): read error: {e}, exiting");
                        break;
                    }
                },
                res = async { wake.as_mut().expect("guarded by is_some").await }, if wake.is_some() => {
                    wake = None;
                    log_tv_result(name, path, "on", res);
                }
            }
        }

        if let Some(w) = wake.take() {
            info!("{name} ({path}): waiting for the wake to finish");
            log_tv_result(name, path, "on", w.await);
        }
        Ok(())
    }

    /// Controller: wake on connect, off on a long press of the Xbox button or idle-off.
    async fn controller(
        cfg: &Config,
        device: &Path,
        dev: Device,
        name: &str,
        path: &str,
    ) -> Result<()> {
        // Resolve our own eventN now: once the device is gone, the node is too.
        let own = own_event_name(device);

        let origin = Instant::now();
        let now = || origin.elapsed();
        let mut machine = Machine::new(now(), cfg.wake_delay(), cfg.long_press(), cfg.idle_off());
        // Bluetooth address, for the idle-off disconnect.
        let mac = dev.unique_name().unwrap_or_default().to_owned();
        let mut events = dev
            .into_event_stream()
            .with_context(|| format!("reading {path}"))?;
        let mut ticker = interval(TICK);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // `tv::on` runs concurrently with the loop, so a slow wake (up to `wake_timeout`)
        // doesn't delay reading events and skew the long-press timestamps.
        let mut wake: Option<TvFuture<'_>> = None;

        info!("{name} ({path}): watching");

        loop {
            let step = tokio::select! {
                res = events.next_event() => match res {
                    Ok(ev) => match btn_mode(ev) {
                        Some(btn_mode) => Step::Event(Event::Input { t: now(), btn_mode }),
                        None => Step::Skip,
                    },
                    Err(e) if e.raw_os_error() == Some(ENODEV) => Step::Event(Event::Gone { t: now() }),
                    Err(e) => Step::Fatal(e),
                },
                _ = ticker.tick(), if machine.needs_tick() => Step::Event(Event::Tick { t: now() }),
                _ = sleep_until(origin + machine.idle_deadline().unwrap_or_default()),
                    if machine.idle_deadline().is_some() =>
                {
                    let game_mode = game_mode().await;
                    if !game_mode {
                        info!("{name} ({path}): idle, but not in Game Mode, leaving it connected");
                    }
                    Step::Event(Event::Idle { t: now(), game_mode })
                }
                res = async { wake.as_mut().expect("guarded by is_some").await }, if wake.is_some() => {
                    Step::WakeDone(res)
                }
            };

            let event = match step {
                Step::Event(event) => event,
                Step::Skip => continue,
                Step::WakeDone(res) => {
                    wake = None;
                    log_tv_result(name, path, "on", res);
                    continue;
                }
                Step::Fatal(e) => {
                    error!("{name} ({path}): read error: {e}, exiting");
                    break;
                }
            };

            let action = machine.step(event);
            let reason = machine
                .reason()
                .map_or_else(|| "unknown".to_owned(), |r| r.to_string());
            match action {
                Action::None => {}
                Action::Wake => match other_controllers(own.as_deref()) {
                    0 => {
                        info!("{name} ({path}): wake ({reason}), turning TV on");
                        wake = Some(Box::pin(tv::on(cfg)));
                    }
                    n => info!(
                        "{name} ({path}): wake ({reason}), but other controller connected ({n}), leaving TV alone"
                    ),
                },
                Action::Off => {
                    // Let a wake still in flight finish first, so `off` doesn't race it.
                    if let Some(w) = wake.take() {
                        log_tv_result(name, path, "on", w.await);
                    }
                    match other_controllers(own.as_deref()) {
                        0 => {
                            info!("{name} ({path}): off ({reason}), turning TV off");
                            log_tv_result(name, path, "off", tv::off(cfg).await);
                        }
                        n => info!(
                            "{name} ({path}): off ({reason}), but other controller connected ({n}), leaving TV on"
                        ),
                    }
                }
                Action::Exit => info!("{name} ({path}): exit ({reason}), no TV action"),
                Action::Disconnect => {
                    info!("{name} ({path}): {reason}, disconnecting {mac}");
                    if let Err(e) = disconnect(&mac).await {
                        error!("{name} ({path}): disconnecting {mac}: {e:#}");
                        machine.disconnect_failed(now());
                    }
                }
            }

            if machine.is_done() {
                break;
            }
        }

        if let Some(w) = wake.take() {
            info!("{name} ({path}): waiting for the wake to finish");
            log_tv_result(name, path, "on", w.await);
        }
        Ok(())
    }

    fn is_key_press(ev: InputEvent) -> bool {
        match ev.destructure() {
            EventSummary::Key(_, code, value) => is_press(code.code(), value),
            _ => false,
        }
    }

    /// Map an event to the machine's `btn_mode` field: `Some(Some(value))` for `BTN_MODE`,
    /// `Some(None)` for any other input, `None` for SYN events (not input on their own; every
    /// real event is followed by a `SYN_REPORT` anyway, and the sync stream already
    /// compensates `SYN_DROPPED` with synthetic events).
    fn btn_mode(ev: InputEvent) -> Option<Option<i32>> {
        match ev.destructure() {
            EventSummary::Synchronization(..) => None,
            EventSummary::Key(_, KeyCode::BTN_MODE, value) => Some(Some(value)),
            _ => Some(None),
        }
    }

    /// Whether the user's display session is Steam's Game Mode. Errors count as no, so the TV
    /// is never turned off on a guess.
    async fn game_mode() -> bool {
        match command(
            "loginctl",
            &["show-session", "auto", "-p", "Desktop", "--value"],
        )
        .await
        {
            Ok(out) => String::from_utf8_lossy(&out.stdout).trim() == GAME_MODE_DESKTOP,
            Err(e) => {
                warn!("checking for Game Mode: {e:#}, assuming not");
                false
            }
        }
    }

    /// Disconnect the controller over Bluetooth, which powers it off.
    async fn disconnect(mac: &str) -> Result<()> {
        anyhow::ensure!(!mac.is_empty(), "the device has no Bluetooth address");
        command("bluetoothctl", &["disconnect", mac])
            .await
            .map(drop)
    }

    /// Run `program` with `args`, failing on a non-zero exit or after [`COMMAND_TIMEOUT`].
    async fn command(program: &str, args: &[&str]) -> Result<Output> {
        let out = timeout(
            COMMAND_TIMEOUT,
            Command::new(program).args(args).kill_on_drop(true).output(),
        )
        .await
        .with_context(|| format!("{program} timed out"))?
        .with_context(|| format!("running {program}"))?;
        anyhow::ensure!(
            out.status.success(),
            "{program} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
        Ok(out)
    }

    /// `eventN` for our device (resolving symlinks like `/dev/input/by-id/...`).
    fn own_event_name(device: &Path) -> Option<OsString> {
        let resolved = device.canonicalize().unwrap_or_else(|_| device.to_owned());
        resolved.file_name().map(ToOwned::to_owned)
    }

    /// Other connected controllers; on a sysfs error, log and assume none.
    fn other_controllers(own: Option<&std::ffi::OsStr>) -> usize {
        count_controllers_in(Path::new(SYSFS_INPUT), own).unwrap_or_else(|e| {
            warn!("counting controllers in {SYSFS_INPUT}: {e}, assuming none");
            0
        })
    }

    fn log_tv_result(name: &str, path: &str, what: &str, res: Result<()>) {
        match res {
            Ok(()) => info!("{name} ({path}): tv {what} done"),
            Err(e) => error!("{name} ({path}): tv {what} failed: {e:#}"),
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::run;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    /// Temp dir that removes itself on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir()
                .join(format!("lgtv-wake-sysfs-{}-{nanos}", std::process::id()));
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

    fn fake_sysfs() -> TempDir {
        let t = TempDir::new();
        t.device("event3", "AT Translated Set 2 keyboard");
        t.device("event17", CONTROLLER_NAME);
        t.device("event21", CONTROLLER_NAME);
        t.device("event22", "Microsoft X-Box 360 pad 0");
        t.device("event23", "Xbox Wireless Controller Consumer Control");
        // Non-event entries for the same controller must not count.
        t.device("input42", CONTROLLER_NAME);
        t.device("js0", CONTROLLER_NAME);
        // An eventN without a readable name (removed meanwhile) is skipped.
        fs::create_dir_all(t.0.join("event30")).unwrap();
        t
    }

    #[test]
    fn counts_other_controllers() {
        let t = fake_sysfs();
        let count = |own: Option<&str>| count_controllers_in(&t.0, own.map(OsStr::new)).unwrap();
        assert_eq!(count(Some("event17")), 1);
        assert_eq!(count(Some("event21")), 1);
        assert_eq!(count(Some("event3")), 2);
        assert_eq!(count(None), 2);
    }

    #[test]
    fn only_controller_counts_zero() {
        let t = TempDir::new();
        t.device("event17", CONTROLLER_NAME);
        t.device("event5", "Power Button");
        assert_eq!(
            count_controllers_in(&t.0, Some(OsStr::new("event17"))).unwrap(),
            0
        );
    }

    #[test]
    fn presses() {
        const KEY_A: u16 = 30;
        const BTN_LEFT: u16 = 0x110;
        const BTN_TOUCH: u16 = 0x14a;
        assert!(is_press(KEY_A, 1));
        assert!(is_press(BTN_LEFT, 1));
        // Releases and autorepeat don't count.
        assert!(!is_press(KEY_A, 0));
        assert!(!is_press(KEY_A, 2));
        assert!(!is_press(BTN_LEFT, 0));
        // Touch contacts aren't clicks.
        assert!(!is_press(BTN_TOUCH, 1));
        assert!(!is_press(0x140, 1));
        assert!(!is_press(0x14f, 1));
        assert!(is_press(0x150, 1));
    }

    #[test]
    fn missing_root_is_an_error() {
        let t = TempDir::new();
        assert!(count_controllers_in(&t.0.join("nope"), None).is_err());
    }
}
