//! `pin`: fetch the TV's certificate and save it to `~/.config/lgtv-wake/tv-cert.der`,
//! which replaces the certificate embedded in the binary (e.g. after a firmware update).

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::config::{self, Config, TlsMode};
use crate::{cert, ssap, tls};

const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn run(cfg: &Config, yes: bool) -> Result<()> {
    let der = match fetch(&cfg.host).await {
        Ok(der) => der,
        Err(e) if ssap::is_unreachable(&e) => bail!(
            "the TV didn't answer at {}:{}: turn it on and try again ({e:#})",
            cfg.host,
            ssap::PORT
        ),
        Err(e) => return Err(e),
    };

    println!("TV certificate from {}:{}:", cfg.host, ssap::PORT);
    print!("{}", describe(&der));

    let current = tls::current_pin()?;
    if current.der == der {
        println!("\nAlready pinned ({}).", current.source());
        return insecure_note(cfg);
    }
    println!(
        "\nCurrently pinned ({}):\n  SHA-256:  {}",
        current.source(),
        cert::fingerprint(&current.der)
    );

    if !yes && !confirm("Pin the TV's certificate instead?")? {
        bail!("not pinned");
    }
    let path = config::cert_path()?;
    save(&path, &der)?;
    println!("Pinned: saved {}", path.display());
    insecure_note(cfg)
}

/// The TV's end-entity certificate (DER).
pub async fn fetch(host: &str) -> Result<Vec<u8>> {
    let host = host.to_owned();
    tokio::task::spawn_blocking(move || tls::fetch_leaf(&host, ssap::PORT, FETCH_TIMEOUT))
        .await
        .context("fetching the certificate")?
}

/// Subject, issuer, validity and fingerprint, one per indented line.
pub fn describe(der: &[u8]) -> String {
    let fingerprint = cert::fingerprint(der);
    match cert::info(der) {
        Ok(info) => format!(
            "  subject:  {}\n  issuer:   {}\n  valid:    {} to {}\n  SHA-256:  {fingerprint}\n",
            info.subject, info.issuer, info.not_before, info.not_after
        ),
        Err(e) => format!("  (could not parse it: {e:#})\n  SHA-256:  {fingerprint}\n"),
    }
}

fn insecure_note(cfg: &Config) -> Result<()> {
    if cfg.tls == TlsMode::Insecure {
        println!(
            "Note: the config has `tls = \"insecure\"`, so the pin isn't used; remove it from {}",
            config::config_path()?.display()
        );
    }
    Ok(())
}

fn confirm(question: &str) -> Result<bool> {
    if !io::stdin().is_terminal() {
        bail!("not a terminal: pass --yes to pin without asking");
    }
    print!("{question} [y/N] ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

/// Write `der` to `path` via a temp file, so a watcher never reads half a certificate.
fn save(path: &std::path::Path, der: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, der)
        .and_then(|()| fs::rename(&tmp, path))
        .with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_embedded_cert() {
        let text = describe(tls::PINNED_CERT);
        assert!(text.contains("CN=LGE TV SSG"), "{text}");
        assert!(text.contains("2034-08-15"), "{text}");
        assert!(text.contains("SHA-256:  11:C5:B1"), "{text}");
    }

    #[test]
    fn describes_garbage() {
        let text = describe(b"junk");
        assert!(text.contains("could not parse"), "{text}");
        assert!(text.contains("SHA-256:"), "{text}");
    }
}
