//! Is there a newer gachi? Asked of GitHub (the project's latest release), once at startup
//! if the setting allows and whenever the settings' "about" page asks.
//!
//! Nothing is downloaded or installed here: the app only says that a newer version exists
//! (its number, nothing else) and opens the download page in the browser.
//!
//! The request is a plain HTTPS GET written out by hand (the app has no HTTP client, and
//! one request does not earn one). It carries nothing about this PC but the app's name.

use std::time::Duration;

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const HOST: &str = "api.github.com";
const PATH: &str = "/repos/aape133/gachi/releases/latest";
/// the whole exchange
const TIMEOUT: Duration = Duration::from_secs(10);
/// more than this is not a release description
const MAX_REPLY: usize = 512 * 1024;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Update {
    /// "0.1.1"
    pub version: String,
}

/// The newer version, if there is one. `Ok(None)`: this is the latest (or there is no
/// release to compare with).
pub async fn check() -> Result<Option<Update>, String> {
    let reply = tokio::time::timeout(TIMEOUT, fetch()).await.map_err(|_| "응답이 없어요".to_string())??;
    let (status, body) = split(&reply).ok_or("알 수 없는 응답이에요")?;
    match status {
        200 => Ok(newer_in(&body, env!("CARGO_PKG_VERSION"))),
        // no release yet (or the repository is not public)
        404 => Ok(None),
        s => Err(format!("확인하지 못했어요 ({s})")),
    }
}

async fn fetch() -> Result<Vec<u8>, String> {
    let cfg = crate::signal::tls_config().ok_or("인증서를 읽지 못했어요")?;
    let tcp = tokio::net::TcpStream::connect((HOST, 443)).await.map_err(|e| e.to_string())?;
    let name = rustls::pki_types::ServerName::try_from(HOST).map_err(|e| e.to_string())?;
    let mut tls = tokio_rustls::TlsConnector::from(cfg).connect(name, tcp).await.map_err(|e| e.to_string())?;
    let req = format!(
        "GET {PATH} HTTP/1.1\r\nHost: {HOST}\r\nUser-Agent: gachi\r\nAccept: application/vnd.github+json\r\nConnection: close\r\n\r\n"
    );
    tls.write_all(req.as_bytes()).await.map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match tls.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if out.len() > MAX_REPLY {
                    return Err("응답이 너무 커요".into());
                }
            }
            // (servers that close without a TLS goodbye: what was read is still the reply)
            Err(_) if !out.is_empty() => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(out)
}

/// An HTTP/1.1 reply's status and body (chunks joined).
fn split(reply: &[u8]) -> Option<(u16, Vec<u8>)> {
    let end = reply.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&reply[..end]).ok()?;
    let body = &reply[end + 4..];
    let mut lines = head.split("\r\n");
    let status = lines.next()?.split(' ').nth(1)?.parse().ok()?;
    let chunked = lines.any(|l| {
        l.split_once(':').is_some_and(|(k, v)| k.trim().eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked"))
    });
    Some((status, if chunked { dechunk(body)? } else { body.to_vec() }))
}

fn dechunk(mut b: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let eol = b.windows(2).position(|w| w == b"\r\n")?;
        let line = std::str::from_utf8(&b[..eol]).ok()?;
        let len = usize::from_str_radix(line.split(';').next()?.trim(), 16).ok()?;
        b = &b[eol + 2..];
        if len == 0 {
            return Some(out);
        }
        if b.len() < len + 2 || out.len() + len > MAX_REPLY {
            return None;
        }
        out.extend_from_slice(&b[..len]);
        b = &b[len + 2..];
    }
}

/// The release in `json` (GitHub's "latest release"), if it is newer than `current`.
fn newer_in(json: &[u8], current: &str) -> Option<Update> {
    let v: serde_json::Value = serde_json::from_slice(json).ok()?;
    if v["draft"].as_bool() == Some(true) || v["prerelease"].as_bool() == Some(true) {
        return None;
    }
    let tag = v["tag_name"].as_str()?;
    let theirs = numbers(tag)?;
    if theirs <= numbers(current)? {
        return None;
    }
    // (written out again from the numbers: the tag's own text is never shown)
    let version = theirs.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(".");
    Some(Update { version })
}

/// "v0.1.10" → [0, 1, 10]. None for anything that is not dotted numbers.
fn numbers(tag: &str) -> Option<Vec<u32>> {
    let t = tag.trim().trim_start_matches(['v', 'V']);
    let v: Option<Vec<u32>> = t.split('.').map(|p| p.parse().ok()).collect();
    v.filter(|v| !v.is_empty() && v.len() <= 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number() {
        assert!(numbers("v0.1.10") > numbers("0.1.9"));
        assert!(numbers("0.2") > numbers("0.1.9"));
        assert_eq!(numbers("v0.1.0"), numbers("0.1.0"));
        assert_eq!(numbers("nightly"), None);
        assert_eq!(numbers("1.0-beta"), None);
        assert_eq!(numbers(""), None);
    }

    #[test]
    fn only_a_newer_release_is_reported() {
        let rel = |tag: &str| format!(r#"{{"tag_name":"{tag}","body":"- 고친 것\r\n- 새 기능\u0007","draft":false,"prerelease":false}}"#);
        assert_eq!(newer_in(rel("v0.1.0").as_bytes(), "0.1.0"), None);
        assert_eq!(newer_in(rel("v0.0.9").as_bytes(), "0.1.0"), None);
        let u = newer_in(rel("v0.1.1").as_bytes(), "0.1.0").unwrap();
        assert_eq!(u.version, "0.1.1");
        assert_eq!(newer_in(br#"{"tag_name":"v9.0.0","prerelease":true}"#, "0.1.0"), None);
        assert_eq!(newer_in(b"<html>", "0.1.0"), None);
        assert_eq!(newer_in(br#"{"message":"Not Found"}"#, "0.1.0"), None);
    }

    #[test]
    fn replies_are_taken_apart() {
        let plain = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
        assert_eq!(split(plain), Some((200, b"{}".to_vec())));
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n{\"a\r\n4;x=1\r\n\":1}\r\n0\r\n\r\n";
        assert_eq!(split(chunked), Some((200, b"{\"a\":1}".to_vec())));
        assert_eq!(split(b"HTTP/1.1 404 Not Found\r\n\r\n").map(|r| r.0), Some(404));
        assert_eq!(split(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nff\r\nshort"), None);
        assert_eq!(split(b"garbage"), None);
    }
}

#[cfg(test)]
mod live {
    /// Asks GitHub for real (run by hand: `cargo test -p gachi live -- --ignored`).
    #[tokio::test]
    #[ignore]
    async fn github_answers() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let reply = super::fetch().await.expect("reply");
        let (status, body) = super::split(&reply).expect("http");
        println!("status {status}, {} bytes", body.len());
        assert!(status == 200 || status == 404);
        println!("{:?}", super::check().await);
        // (what a 0.0.1 would be told)
        println!("{:?}", super::newer_in(&body, "0.0.1"));
    }
}
