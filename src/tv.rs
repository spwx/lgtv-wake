//! `pair` / `on` / `off` / `status`, built on `ssap` and `wol`.

use std::fmt;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::config::{self, Config};
use crate::ssap::{self, Client};
use crate::{lock, wol};

/// Connect timeout when the TV is expected to be on (`pair`, `status`, `off`).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// `on`: quick check whether the TV is already on before waking it.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// `on`: per-attempt connect timeout while waking.
const WAKE_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// `on`: pause between connect attempts while waking.
const WAKE_BACKOFF: Duration = Duration::from_secs(1);
/// `on`: resend the magic packet this often while waking.
const WAKE_RESEND: Duration = Duration::from_secs(3);
/// How long `pair` waits for the prompt to be accepted.
const PAIR_TIMEOUT: Duration = Duration::from_secs(60);
/// Registration with a stored key (no prompt expected).
const REGISTER_TIMEOUT: Duration = Duration::from_secs(10);

const POWER_STATE: &str = "ssap://com.webos.service.tvpower/power/getPowerState";
const FOREGROUND_APP: &str = "ssap://com.webos.applicationManager/getForegroundAppInfo";
const SWITCH_INPUT: &str = "ssap://tv/switchInput";
const TURN_OFF: &str = "ssap://system/turnOff";

const INPUT_APP_PREFIX: &str = "com.webos.app.";

/// The command already printed why it failed; `main` should exit non-zero quietly.
#[derive(Debug)]
pub struct Reported;

impl fmt::Display for Reported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("already reported")
    }
}

impl std::error::Error for Reported {}

/// Register without a key, wait for the prompt to be accepted, save the key.
/// Refuses to overwrite an existing key unless `force`.
pub async fn pair(cfg: &Config, force: bool) -> Result<()> {
    let path = config::key_path()?;
    if !force && config::load_client_key()?.is_some() {
        bail!(
            "already paired: a client key exists at {}\nRun `lgtv-wake pair --force` to pair again and replace it",
            path.display()
        );
    }
    let mut client = Client::connect(cfg, CONNECT_TIMEOUT)
        .await
        .context("could not reach the TV (it must be on to pair)")?;
    println!("Accept the prompt on the TV");
    let key = client.register(None, PAIR_TIMEOUT).await?;
    client.close().await;
    config::save_client_key(&key)?;
    println!("Paired. Client key saved to {}", path.display());
    Ok(())
}

/// Wake the TV if needed and switch to `cfg.input`.
pub async fn on(cfg: &Config) -> Result<()> {
    let key = require_key()?;
    let _lock = lock::acquire().await?;

    let start = Instant::now();
    let (mut client, woke) = match Client::connect(cfg, PROBE_TIMEOUT).await {
        Ok(client) => {
            info!("TV is already on");
            (client, false)
        }
        Err(e) if ssap::is_unreachable(&e) => {
            debug!("TV not answering ({e:#}), waking it");
            let client = wake(cfg, start).await?;
            info!("TV woke up after {:.1}s", start.elapsed().as_secs_f64());
            (client, true)
        }
        Err(e) => return Err(e),
    };

    client.register(Some(&key), REGISTER_TIMEOUT).await?;

    if woke {
        match client.request(POWER_STATE, json!({})).await {
            Ok(p) => {
                let state = power_state(&p);
                // Seen: "Suspend" right after a wake, then "Active" ~3s later; the
                // input switch works either way.
                if state != "Active" {
                    info!(
                        "TV power state after wake is {state:?} (usually becomes \"Active\" within seconds)"
                    );
                }
            }
            Err(e) => warn!("could not read the power state after wake: {e:#}"),
        }
    } else {
        // Skip the switch when it's already on the input: keyboard and mouse wakes repeat
        // this while the TV is in use.
        match client.request(FOREGROUND_APP, json!({})).await {
            Ok(app) if app_id(&app) == input_app_id(&cfg.input) => {
                info!("already on {}", cfg.input);
                client.close().await;
                return Ok(());
            }
            Ok(_) => {}
            Err(e) => warn!("could not read the current input: {e:#}"),
        }
    }

    client
        .request(SWITCH_INPUT, json!({ "inputId": cfg.input }))
        .await?;
    info!(
        "switched to {} ({:.1}s total)",
        cfg.input,
        start.elapsed().as_secs_f64()
    );
    client.close().await;
    Ok(())
}

/// Send magic packets and retry connecting until the TV answers or `wake_timeout` passes.
async fn wake(cfg: &Config, start: Instant) -> Result<Client> {
    let deadline = start + cfg.wake_timeout();
    let mut last_packet: Option<Instant> = None;
    loop {
        if last_packet.is_none_or(|t| t.elapsed() >= WAKE_RESEND) {
            wol::send(&cfg.mac, cfg.broadcast).await?;
            debug!("sent magic packet");
            last_packet = Some(Instant::now());
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        let attempt = WAKE_CONNECT_TIMEOUT.min(remaining);
        let err = match Client::connect(cfg, attempt).await {
            Ok(client) => return Ok(client),
            Err(e) => e,
        };
        if !ssap::is_unreachable(&err) {
            // e.g. a TLS certificate mismatch: retrying won't help.
            return Err(err);
        }
        debug!("not answering yet: {err:#}");

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(err).context(format!(
                "TV did not answer within {}s of Wake-on-LAN",
                cfg.wake_timeout_secs
            ));
        }
        tokio::time::sleep(WAKE_BACKOFF.min(remaining)).await;
    }
}

/// Turn the TV off if it's reachable and on `cfg.input`.
pub async fn off(cfg: &Config) -> Result<()> {
    let key = require_key()?;
    let _lock = lock::acquire().await?;

    let mut client = match Client::connect(cfg, CONNECT_TIMEOUT).await {
        Ok(client) => client,
        Err(e) if ssap::is_unreachable(&e) => {
            info!("TV already off");
            debug!("{e:#}");
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    client.register(Some(&key), REGISTER_TIMEOUT).await?;

    let app = client.request(FOREGROUND_APP, json!({})).await?;
    let app_id = app_id(&app);
    let expected = input_app_id(&cfg.input);
    if app_id == expected {
        client.request(TURN_OFF, json!({})).await?;
        info!("TV on {} ({app_id}), turned it off", cfg.input);
    } else {
        info!(
            "TV is showing {} rather than {} ({expected}), leaving it on",
            if app_id.is_empty() { "nothing" } else { app_id },
            cfg.input
        );
    }
    client.close().await;
    Ok(())
}

/// Print power state and current input; error if the TV doesn't answer.
pub async fn status(cfg: &Config) -> Result<()> {
    let key = require_key()?;
    let mut client = match Client::connect(cfg, CONNECT_TIMEOUT).await {
        Ok(client) => client,
        Err(e) if ssap::is_unreachable(&e) => {
            debug!("{e:#}");
            println!("power: off/standby (no response)");
            return Err(Reported.into());
        }
        Err(e) => return Err(e),
    };
    client.register(Some(&key), REGISTER_TIMEOUT).await?;
    let power = client.request(POWER_STATE, json!({})).await?;
    let app = client.request(FOREGROUND_APP, json!({})).await?;
    client.close().await;
    println!("{}", status_line(&power, &app));
    Ok(())
}

/// The stored client key, or an error telling the user to pair.
fn require_key() -> Result<String> {
    match config::load_client_key()? {
        Some(key) => Ok(key),
        None => bail!(
            "not paired yet: no client key at {}\nRun `lgtv-wake pair` (with the TV on) first",
            config::key_path()?.display()
        ),
    }
}

/// webOS app id shown for an input: `HDMI_1` → `com.webos.app.hdmi1`.
pub fn input_app_id(input: &str) -> String {
    let name: String = input
        .chars()
        .filter(|&c| c != '_')
        .map(|c| c.to_ascii_lowercase())
        .collect();
    format!("{INPUT_APP_PREFIX}{name}")
}

/// Input for an input app id: `com.webos.app.hdmi1` → `HDMI_1`; `None` for other apps.
pub fn app_input(app_id: &str) -> Option<String> {
    let name = app_id.strip_prefix(INPUT_APP_PREFIX)?;
    let digits = name.strip_prefix("hdmi")?;
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| format!("HDMI_{digits}"))
}

fn power_state(payload: &Value) -> &str {
    payload["state"].as_str().unwrap_or("unknown")
}

fn app_id(payload: &Value) -> &str {
    payload["appId"].as_str().unwrap_or_default()
}

/// `power: Active, input: HDMI_1 (com.webos.app.hdmi1)`.
fn status_line(power: &Value, app: &Value) -> String {
    let mut power_text = power_state(power).to_owned();
    if let Some(processing) = power["processing"].as_str().filter(|p| *p != power_text) {
        power_text = format!("{power_text} ({processing})");
    }
    let app_id = app_id(app);
    let input = match app_input(app_id) {
        Some(input) => format!("input: {input} ({app_id})"),
        None if app_id.is_empty() => "app: none".to_owned(),
        None => format!("app: {app_id}"),
    };
    format!("power: {power_text}, {input}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_to_app_id() {
        assert_eq!(input_app_id("HDMI_1"), "com.webos.app.hdmi1");
        assert_eq!(input_app_id("HDMI_2"), "com.webos.app.hdmi2");
        assert_eq!(input_app_id("hdmi_4"), "com.webos.app.hdmi4");
        assert_eq!(input_app_id("HDMI1"), "com.webos.app.hdmi1");
    }

    #[test]
    fn app_id_to_input() {
        assert_eq!(app_input("com.webos.app.hdmi1").as_deref(), Some("HDMI_1"));
        assert_eq!(app_input("com.webos.app.hdmi3").as_deref(), Some("HDMI_3"));
        assert_eq!(app_input("com.webos.app.hdmi"), None);
        assert_eq!(app_input("com.webos.app.livetv"), None);
        assert_eq!(app_input("netflix"), None);
        assert_eq!(app_input(""), None);
        for input in ["HDMI_1", "HDMI_2", "HDMI_3", "HDMI_4"] {
            assert_eq!(app_input(&input_app_id(input)).as_deref(), Some(input));
        }
    }

    #[test]
    fn status_lines() {
        let active = json!({"returnValue": true, "state": "Active"});
        let hdmi1 = json!({"returnValue": true, "appId": "com.webos.app.hdmi1"});
        assert_eq!(
            status_line(&active, &hdmi1),
            "power: Active, input: HDMI_1 (com.webos.app.hdmi1)"
        );
        let screen_off = json!({"state": "Screen Off", "processing": "Screen Off"});
        let netflix = json!({"appId": "netflix"});
        assert_eq!(
            status_line(&screen_off, &netflix),
            "power: Screen Off, app: netflix"
        );
        let standby = json!({"state": "Active Standby", "processing": "Request Power Off"});
        assert_eq!(
            status_line(&standby, &hdmi1),
            "power: Active Standby (Request Power Off), input: HDMI_1 (com.webos.app.hdmi1)"
        );
        assert_eq!(
            status_line(&json!({}), &json!({"appId": ""})),
            "power: unknown, app: none"
        );
    }
}
