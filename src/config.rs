//! Configuration file and client key, both in `~/.config/lgtv-wake/`.

use std::fs;
use std::io::{ErrorKind, Write};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Deserializer};

/// Template printed when the config file is missing.
pub const TEMPLATE: &str = r#"host = "192.168.1.50"
mac = "aa:bb:cc:dd:ee:ff"
broadcast = "192.168.1.255"
input = "HDMI_1"
# optional, with these defaults:
wake_delay_secs = 5
long_press_secs = 5
wake_timeout_secs = 20
idle_off_mins = 15   # Game Mode only; 0 disables
off_grace_secs = 30  # ignore keyboard and mouse this long after turning the TV off
# tls = "insecure"   # accept any certificate from `host` (if the pinned one changed)
"#;

const CONFIG_FILE: &str = "config.toml";
const KEY_FILE: &str = "client-key";
const CERT_FILE: &str = "tv-cert.der";

/// How the TV's TLS certificate is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    /// Accept only the pinned certificate (`lgtv-wake pin`'s, else the embedded one).
    #[default]
    Pinned,
    /// Accept any certificate from the configured host.
    Insecure,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// TV address (IP preferred, so DNS can't block a wake).
    pub host: String,
    /// TV MAC address, for Wake-on-LAN.
    #[serde(deserialize_with = "deserialize_mac")]
    pub mac: [u8; 6],
    /// Subnet broadcast address for the magic packet.
    pub broadcast: Ipv4Addr,
    /// Input to switch to, e.g. `HDMI_1`.
    pub input: String,
    #[serde(default = "default_wake_delay")]
    pub wake_delay_secs: u64,
    #[serde(default = "default_long_press")]
    pub long_press_secs: u64,
    #[serde(default = "default_wake_timeout")]
    pub wake_timeout_secs: u64,
    /// Disconnect a controller idle this long in Game Mode (0 disables).
    #[serde(default = "default_idle_off")]
    pub idle_off_mins: u64,
    /// Ignore keyboard and mouse presses this long after `off` turned the TV off, so a bump
    /// while it shuts down doesn't wake it again (0 disables).
    #[serde(default = "default_off_grace")]
    pub off_grace_secs: u64,
    #[serde(default)]
    pub tls: TlsMode,
}

fn default_wake_delay() -> u64 {
    5
}
fn default_long_press() -> u64 {
    5
}
fn default_wake_timeout() -> u64 {
    20
}
fn default_idle_off() -> u64 {
    15
}
fn default_off_grace() -> u64 {
    30
}

impl Config {
    /// Load from `~/.config/lgtv-wake/config.toml`.
    pub fn load() -> Result<Self> {
        Self::load_from(&config_path()?)
    }

    /// Load from a specific path. A missing file is an error that includes [`TEMPLATE`].
    pub fn load_from(path: &Path) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound => bail!(
                "config file not found: {}\nCreate it with contents like:\n\n{TEMPLATE}",
                path.display()
            ),
            Err(e) => {
                return Err(e).with_context(|| format!("reading {}", path.display()));
            }
        };
        Self::parse(&text).with_context(|| format!("invalid config file {}", path.display()))
    }

    /// Parse TOML text.
    pub fn parse(text: &str) -> Result<Self> {
        Ok(toml::from_str(text)?)
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))] // used by `watch`
    pub fn wake_delay(&self) -> Duration {
        Duration::from_secs(self.wake_delay_secs)
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))] // used by `watch`
    pub fn long_press(&self) -> Duration {
        Duration::from_secs(self.long_press_secs)
    }

    /// `None` when `idle_off_mins` is 0.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))] // used by `watch`
    pub fn idle_off(&self) -> Option<Duration> {
        (self.idle_off_mins > 0).then(|| Duration::from_secs(self.idle_off_mins * 60))
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))] // used by `watch`
    pub fn off_grace(&self) -> Duration {
        Duration::from_secs(self.off_grace_secs)
    }

    pub fn wake_timeout(&self) -> Duration {
        Duration::from_secs(self.wake_timeout_secs)
    }
}

/// Parse `aa:bb:cc:dd:ee:ff` (or `-` separated) into bytes.
pub fn parse_mac(s: &str) -> Result<[u8; 6]> {
    let err =
        || anyhow!("invalid MAC address {s:?}: expected six hex bytes like \"aa:bb:cc:dd:ee:ff\"");
    let parts: Vec<&str> = s.trim().split([':', '-']).collect();
    if parts.len() != 6 {
        return Err(err());
    }
    let mut mac = [0u8; 6];
    for (byte, part) in mac.iter_mut().zip(parts) {
        if part.len() != 2 {
            return Err(err());
        }
        *byte = u8::from_str_radix(part, 16).map_err(|_| err())?;
    }
    Ok(mac)
}

fn deserialize_mac<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 6], D::Error> {
    let s = String::deserialize(d)?;
    parse_mac(&s).map_err(serde::de::Error::custom)
}

/// `~/.config/lgtv-wake` (`$XDG_CONFIG_HOME/lgtv-wake` on Linux).
pub fn config_dir() -> Result<PathBuf> {
    let base = if cfg!(target_os = "linux") {
        dirs::config_dir()
    } else {
        // dirs::config_dir() is ~/Library/Application Support on macOS; use ~/.config everywhere.
        dirs::home_dir().map(|h| h.join(".config"))
    };
    Ok(base
        .context("could not determine the home/config directory")?
        .join("lgtv-wake"))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join(CONFIG_FILE))
}

pub fn key_path() -> Result<PathBuf> {
    Ok(config_dir()?.join(KEY_FILE))
}

/// The TV certificate saved by `lgtv-wake pin`, used instead of the embedded one.
pub fn cert_path() -> Result<PathBuf> {
    Ok(config_dir()?.join(CERT_FILE))
}

/// Load the client key from `~/.config/lgtv-wake/client-key`, `None` if not paired yet.
pub fn load_client_key() -> Result<Option<String>> {
    load_client_key_from(&key_path()?)
}

pub fn load_client_key_from(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(s) => {
            let key = s.trim();
            Ok((!key.is_empty()).then(|| key.to_owned()))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Save the client key with mode 0600, creating the directory if needed.
pub fn save_client_key(key: &str) -> Result<()> {
    save_client_key_to(&key_path()?, key)
}

pub fn save_client_key_to(path: &Path, key: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    // Write to a temp file created 0600, then rename over the target.
    let tmp = path.with_extension("tmp");
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let write = || -> std::io::Result<()> {
        let mut f = opts.open(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // In case the temp file already existed with other permissions.
            f.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        writeln!(f, "{key}")?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    };
    write().with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
host = "192.168.11.232"
mac = "d0:cd:bf:66:69:c2"
broadcast = "192.168.11.255"
input = "HDMI_1"
"#;

    #[test]
    fn defaults() {
        let c = Config::parse(MINIMAL).unwrap();
        assert_eq!(c.host, "192.168.11.232");
        assert_eq!(c.mac, [0xd0, 0xcd, 0xbf, 0x66, 0x69, 0xc2]);
        assert_eq!(c.broadcast, Ipv4Addr::new(192, 168, 11, 255));
        assert_eq!(c.input, "HDMI_1");
        assert_eq!(c.wake_delay_secs, 5);
        assert_eq!(c.long_press_secs, 5);
        assert_eq!(c.wake_timeout_secs, 20);
        assert_eq!(c.idle_off(), Some(Duration::from_secs(900)));
        assert_eq!(c.off_grace(), Duration::from_secs(30));
        assert_eq!(c.tls, TlsMode::Pinned);
    }

    #[test]
    fn template_parses() {
        // The template spells out the defaults, so it matches a minimal config
        // with the same required values.
        let c = Config::parse(TEMPLATE).unwrap();
        let minimal = MINIMAL
            .replace("192.168.11.232", "192.168.1.50")
            .replace("d0:cd:bf:66:69:c2", "aa:bb:cc:dd:ee:ff")
            .replace("192.168.11.255", "192.168.1.255");
        assert_eq!(c, Config::parse(&minimal).unwrap());
    }

    #[test]
    fn overrides_and_insecure_tls() {
        let text = format!(
            "{MINIMAL}wake_delay_secs = 7\nlong_press_secs = 3\nwake_timeout_secs = 30\nidle_off_mins = 0\noff_grace_secs = 0\ntls = \"insecure\"\n"
        );
        let c = Config::parse(&text).unwrap();
        assert_eq!(c.wake_delay(), Duration::from_secs(7));
        assert_eq!(c.long_press(), Duration::from_secs(3));
        assert_eq!(c.wake_timeout(), Duration::from_secs(30));
        assert_eq!(c.idle_off(), None);
        assert_eq!(c.off_grace(), Duration::ZERO);
        assert_eq!(c.tls, TlsMode::Insecure);
    }

    #[test]
    fn bad_tls_mode() {
        let text = format!("{MINIMAL}tls = \"off\"\n");
        assert!(Config::parse(&text).is_err());
    }

    #[test]
    fn bad_mac() {
        let text = MINIMAL.replace("d0:cd:bf:66:69:c2", "d0:cd:bf:66:69");
        let err = format!("{:#}", Config::parse(&text).unwrap_err());
        assert!(err.contains("invalid MAC address"), "{err}");
    }

    #[test]
    fn parse_mac_variants() {
        assert_eq!(
            parse_mac("D0-CD-BF-66-69-C2").unwrap(),
            [0xd0, 0xcd, 0xbf, 0x66, 0x69, 0xc2]
        );
        assert!(parse_mac("d0:cd:bf:66:69:zz").is_err());
        assert!(parse_mac("d0:cd:bf:66:69:c2:00").is_err());
        assert!(parse_mac("d0:cd:bf:66:69:c").is_err());
        assert!(parse_mac("").is_err());
    }

    #[test]
    fn missing_field_and_unknown_field() {
        let no_host = MINIMAL.replace("host = \"192.168.11.232\"\n", "");
        assert!(Config::parse(&no_host).is_err());
        let typo = format!("{MINIMAL}wake_dealy_secs = 5\n");
        assert!(Config::parse(&typo).is_err());
    }

    #[test]
    fn missing_file_prints_template() {
        let dir =
            std::env::temp_dir().join(format!("lgtv-wake-test-missing-{}", std::process::id()));
        let err = format!(
            "{:#}",
            Config::load_from(&dir.join("config.toml")).unwrap_err()
        );
        assert!(err.contains("config file not found"), "{err}");
        assert!(err.contains("wake_timeout_secs = 20"), "{err}");
    }

    #[test]
    fn client_key_roundtrip() {
        let dir = std::env::temp_dir().join(format!("lgtv-wake-test-key-{}", std::process::id()));
        let path = dir.join("sub").join("client-key");
        assert_eq!(load_client_key_from(&path).unwrap(), None);
        save_client_key_to(&path, "abc123").unwrap();
        assert_eq!(
            load_client_key_from(&path).unwrap().as_deref(),
            Some("abc123")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        fs::remove_dir_all(&dir).unwrap();
    }
}
