//! `update`: download the latest release and run its `setup` (Linux).
//!
//! The download is checked against the release's `.sha256` file, then the new
//! binary's `setup` installs itself and refreshes the unit and the udev rule,
//! keeping the config and client key.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::setup;

const REPO: &str = "https://github.com/spwx/lgtv-wake";
const ASSET: &str = "lgtv-wake-x86_64-linux-musl";
const CURRENT: &str = concat!("v", env!("CARGO_PKG_VERSION"));

#[derive(Debug, clap::Args)]
pub struct Options {
    /// Reinstall even if this is already the latest version
    #[arg(long)]
    force: bool,
    /// Print the commands that need root instead of running them with sudo
    #[arg(long)]
    no_sudo: bool,
}

pub fn run(opts: &Options) -> Result<()> {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        bail!("update is only supported on x86_64 Linux");
    }
    let tag = latest_tag()?;
    if tag == CURRENT && !opts.force {
        println!("already up to date ({CURRENT})");
        return Ok(());
    }
    println!("updating {CURRENT} -> {tag}");

    // Download next to the installed binary: /tmp may be mounted noexec.
    let download = setup::installed_path()?.with_extension("download");
    let dir = download.parent().expect("install path has a parent");
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let result = download_and_setup(&tag, &download, opts.no_sudo);
    let _ = fs::remove_file(&download);
    result
}

fn download_and_setup(tag: &str, path: &Path, no_sudo: bool) -> Result<()> {
    let url = format!("{REPO}/releases/download/{tag}/{ASSET}");
    let path_str = path.to_str().context("download path is not UTF-8")?;
    setup::run_cmd("curl", &["-fsSLo", path_str, &url])?;
    let sums = curl(&format!("{url}.sha256"))?;
    let expected = sums
        .split_whitespace()
        .next()
        .context("empty checksum file")?;
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let actual = hex(ring::digest::digest(&ring::digest::SHA256, &bytes).as_ref());
    if !actual.eq_ignore_ascii_case(expected) {
        bail!("checksum mismatch for {ASSET}: expected {expected}, got {actual}");
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;

    let mut args = vec!["setup"];
    if no_sudo {
        args.push("--no-sudo");
    }
    setup::run_cmd(path_str, &args)
}

/// The latest release's tag, from where `releases/latest` redirects.
fn latest_tag() -> Result<String> {
    let url = curl_args(&[
        "-fsSLo",
        "/dev/null",
        "-w",
        "%{url_effective}",
        &format!("{REPO}/releases/latest"),
    ])?;
    tag_from_url(&url).with_context(|| format!("no release found (redirected to {url})"))
}

fn tag_from_url(url: &str) -> Option<String> {
    let (_, tag) = url.trim().rsplit_once("/releases/tag/")?;
    (!tag.is_empty()).then(|| tag.to_string())
}

fn curl(url: &str) -> Result<String> {
    curl_args(&["-fsSL", url])
}

fn curl_args(args: &[&str]) -> Result<String> {
    let out = Command::new("curl")
        .args(args)
        .output()
        .context("running curl")?;
    if !out.status.success() {
        bail!(
            "`curl {}` failed ({}): {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    String::from_utf8(out.stdout).context("curl output is not UTF-8")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_from_redirect() {
        assert_eq!(
            tag_from_url("https://github.com/spwx/lgtv-wake/releases/tag/v0.1.3\n").as_deref(),
            Some("v0.1.3")
        );
        assert_eq!(
            tag_from_url("https://github.com/spwx/lgtv-wake/releases"),
            None
        );
    }

    #[test]
    fn sha256_hex() {
        let digest = ring::digest::digest(&ring::digest::SHA256, b"abc");
        assert_eq!(
            hex(digest.as_ref()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
