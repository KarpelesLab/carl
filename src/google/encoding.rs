//! Small encoding helpers the Google feature needs, kept dependency-free
//! (base64url and randomness come from purecrypto, already linked for TLS).

use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Result, anyhow};
use purecrypto::jose::base64url;
use purecrypto::rng::{OsRng, RngCore};

/// Percent-encode everything except RFC 3986 unreserved characters. Safe for
/// both query values and path segments (e.g. calendar ids with `@` and `#`).
pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `k1=v1&k2=v2`, percent-encoded.
pub fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Parse a query string (`a=1&b=x%20y`, `+` as space) into a map.
pub fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (url_decode(k), url_decode(v))
        })
        .collect()
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = s
                    .get(i + 1..i + 3)
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(b) => {
                        out.push(b);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `n` random bytes as unpadded base64url.
pub fn random_token(n: usize) -> String {
    let mut buf = vec![0u8; n];
    OsRng.fill_bytes(&mut buf);
    base64url::encode(&buf)
}

pub fn b64url_encode(data: &[u8]) -> String {
    base64url::encode(data)
}

/// Decode base64url, tolerating the padding some Google APIs include.
pub fn b64url_decode(s: &str) -> Result<Vec<u8>> {
    base64url::decode(s.trim_end_matches('=')).map_err(|e| anyhow!("bad base64url: {e}"))
}

/// Standard, padded base64 (RFC 4648 §4), as MIME encoded-words need.
pub fn b64_std_encode(data: &[u8]) -> String {
    let mut s: String = base64url::encode(data)
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            c => c,
        })
        .collect();
    while !s.len().is_multiple_of(4) {
        s.push('=');
    }
    s
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `unix` seconds as an RFC 3339 UTC timestamp, e.g. `2026-09-29T10:00:00Z`.
pub fn rfc3339(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let secs = unix % 86_400;
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_round_trip() {
        let s = "a b&c=d/é#x@y";
        let enc = url_encode(s);
        assert_eq!(enc, "a%20b%26c%3Dd%2F%C3%A9%23x%40y");
        assert_eq!(parse_query(&format!("k={enc}&e=&p=1+2"))["k"], s);
        assert_eq!(parse_query("p=1+2&bad=%zz")["p"], "1 2");
        assert_eq!(parse_query("bad=%zz")["bad"], "%zz");
    }

    #[test]
    fn base64_variants() {
        assert_eq!(b64_std_encode(b"\xfb\xff"), "+/8=");
        assert_eq!(b64_std_encode(b"hello"), "aGVsbG8=");
        assert_eq!(b64url_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(
            b64url_decode(&b64url_encode(b"\xfb\xff")).unwrap(),
            b"\xfb\xff"
        );
    }

    #[test]
    fn rfc3339_formatting() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_790_674_931), "2026-09-29T09:42:11Z");
    }
}
