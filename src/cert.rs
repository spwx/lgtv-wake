//! Just enough X.509 DER parsing to show a certificate's subject, issuer and validity,
//! plus its SHA-256 fingerprint. Nothing here is used to decide whether to trust it.

use anyhow::{Context, Result, bail};

/// What `pin` and `doctor` print about a certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    pub subject: String,
    pub issuer: String,
    pub not_before: String,
    pub not_after: String,
}

/// Parse the parts of `der` that [`Info`] shows.
pub fn info(der: &[u8]) -> Result<Info> {
    let mut input = der;
    let mut cert = read(&mut input, SEQUENCE).context("certificate")?;
    let mut tbs = read(&mut cert, SEQUENCE).context("tbsCertificate")?;
    if tbs.first() == Some(&VERSION) {
        read(&mut tbs, VERSION)?;
    }
    read(&mut tbs, INTEGER).context("serial number")?;
    read(&mut tbs, SEQUENCE).context("signature algorithm")?;
    let issuer = name(read(&mut tbs, SEQUENCE).context("issuer")?)?;
    let mut validity = read(&mut tbs, SEQUENCE).context("validity")?;
    let not_before = time(&mut validity).context("notBefore")?;
    let not_after = time(&mut validity).context("notAfter")?;
    let subject = name(read(&mut tbs, SEQUENCE).context("subject")?)?;
    Ok(Info {
        subject,
        issuer,
        not_before,
        not_after,
    })
}

/// SHA-256 of `der` as colon-separated uppercase hex, like `openssl x509 -fingerprint`.
pub fn fingerprint(der: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, der)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

const INTEGER: u8 = 0x02;
const OID: u8 = 0x06;
const SEQUENCE: u8 = 0x30;
const SET: u8 = 0x31;
const UTC_TIME: u8 = 0x17;
const GENERALIZED_TIME: u8 = 0x18;
/// `[0] EXPLICIT Version`.
const VERSION: u8 = 0xA0;

/// Read one DER element with tag `tag` from the front of `input`, returning its contents.
fn read<'a>(input: &mut &'a [u8], tag: u8) -> Result<&'a [u8]> {
    let (found, body) = read_any(input)?;
    if found != tag {
        bail!("expected tag {tag:#04x}, found {found:#04x}");
    }
    Ok(body)
}

/// Read one DER element (single-byte tag) from the front of `input`.
fn read_any<'a>(input: &mut &'a [u8]) -> Result<(u8, &'a [u8])> {
    let &[tag, first, ref rest @ ..] = *input else {
        bail!("truncated");
    };
    let (len, rest) = match first {
        0..=0x7F => (first as usize, rest),
        0x81..=0x84 => {
            let n = (first & 0x7F) as usize;
            if rest.len() < n {
                bail!("truncated length");
            }
            let len = rest[..n]
                .iter()
                .fold(0usize, |acc, &b| acc << 8 | b as usize);
            (len, &rest[n..])
        }
        _ => bail!("unsupported length encoding {first:#04x}"),
    };
    if rest.len() < len {
        bail!("truncated");
    }
    let (body, rest) = rest.split_at(len);
    *input = rest;
    Ok((tag, body))
}

/// An X.501 Name as `C=KR, O=..., CN=...`, in encoded order.
fn name(mut rdns: &[u8]) -> Result<String> {
    let mut parts = Vec::new();
    while !rdns.is_empty() {
        let mut set = read(&mut rdns, SET)?;
        while !set.is_empty() {
            let mut attr = read(&mut set, SEQUENCE)?;
            let oid = read(&mut attr, OID)?;
            let (_, value) = read_any(&mut attr)?;
            parts.push(format!(
                "{}={}",
                attr_name(oid),
                String::from_utf8_lossy(value)
            ));
        }
    }
    Ok(parts.join(", "))
}

fn attr_name(oid: &[u8]) -> String {
    match oid {
        [0x55, 0x04, 0x03] => "CN".into(),
        [0x55, 0x04, 0x06] => "C".into(),
        [0x55, 0x04, 0x07] => "L".into(),
        [0x55, 0x04, 0x08] => "ST".into(),
        [0x55, 0x04, 0x0A] => "O".into(),
        [0x55, 0x04, 0x0B] => "OU".into(),
        _ => dotted(oid),
    }
}

/// An OID's contents in dotted form, e.g. `1.2.840.113549.1.9.1`.
fn dotted(oid: &[u8]) -> String {
    let Some((&first, rest)) = oid.split_first() else {
        return String::new();
    };
    let mut arcs = vec![u64::from(first / 40), u64::from(first % 40)];
    let mut arc = 0u64;
    for &b in rest {
        arc = arc << 7 | u64::from(b & 0x7F);
        if b & 0x80 == 0 {
            arcs.push(arc);
            arc = 0;
        }
    }
    arcs.iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(".")
}

/// A UTCTime or GeneralizedTime as `YYYY-MM-DD HH:MM:SS UTC`.
fn time(input: &mut &[u8]) -> Result<String> {
    let (tag, body) = read_any(input)?;
    let text = std::str::from_utf8(body).context("time is not ASCII")?;
    let full = match tag {
        // RFC 5280: YY >= 50 is 19YY, otherwise 20YY.
        UTC_TIME => {
            let yy: u32 = text.get(..2).context("short time")?.parse()?;
            format!("{}{text}", if yy >= 50 { "19" } else { "20" })
        }
        GENERALIZED_TIME => text.to_owned(),
        _ => bail!("expected a time, found tag {tag:#04x}"),
    };
    let digits = full.strip_suffix('Z').context("time not in UTC")?;
    if digits.len() != 14 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        bail!("unexpected time format {text:?}");
    }
    Ok(format!(
        "{}-{}-{} {}:{}:{} UTC",
        &digits[..4],
        &digits[4..6],
        &digits[6..8],
        &digits[8..10],
        &digits[10..12],
        &digits[12..14]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::PINNED_CERT;

    #[test]
    fn embedded_cert_info() {
        let info = info(PINNED_CERT).unwrap();
        assert!(info.subject.ends_with("CN=LGE TV SSG"), "{info:?}");
        assert!(
            info.issuer.ends_with("CN=LGE SSG Intermediate CA"),
            "{info:?}"
        );
        assert!(info.not_before.starts_with("2018-03-12 "), "{info:?}");
        assert!(info.not_after.starts_with("2034-08-15 "), "{info:?}");
    }

    #[test]
    fn embedded_cert_fingerprint() {
        assert_eq!(
            fingerprint(PINNED_CERT),
            "11:C5:B1:C5:90:77:50:AB:B9:DA:2A:66:65:CC:CE:2B:B2:88:A5:83:F4:5A:33:39:E7:1F:87:BF:2F:80:85:52"
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(info(b"not a certificate").is_err());
        assert!(info(&PINNED_CERT[..100]).is_err());
    }

    #[test]
    fn oid_fallback() {
        // 1.2.840.113549.1.9.1 (emailAddress)
        assert_eq!(
            attr_name(&[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x01]),
            "1.2.840.113549.1.9.1"
        );
    }

    #[test]
    fn times() {
        let mut utc: &[u8] = b"\x17\x0d180312093000Z";
        assert_eq!(time(&mut utc).unwrap(), "2018-03-12 09:30:00 UTC");
        let mut gen_time: &[u8] = b"\x18\x0f20500101000000Z";
        assert_eq!(time(&mut gen_time).unwrap(), "2050-01-01 00:00:00 UTC");
    }
}
