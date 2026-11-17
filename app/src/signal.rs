//! WebSocket client for the connection server (Cloudflare Worker).
//!
//! The server setting may be `https://…`, `wss://…`, `ws://…` or a plain `host[:port]`
//! (e.g. `gachi.you.workers.dev`). Plain addresses use TLS unless they point at this
//! computer (local `wrangler dev`): passwords travel over this connection.

use futures_util::{SinkExt, StreamExt};
use gachi_proto::signal::{FromWorker, ToWorker};
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio_tungstenite::tungstenite::Message;

/// Keepalive: the server answers "ping" with "pong" without waking up. Every 5 s: a
/// connection that died on the way (a router that forgot it: seen ten times in an hour on
/// one network) is only noticed when we next send on it, and until then the host is not
/// reached by anyone asking to join. At 20 s that was up to 20 s of "the host does not
/// answer"; the server noticed such a connection 17 minutes later.
const KEEPALIVE: Duration = Duration::from_secs(5);

/// How long a connection we are done with may take to finish closing.
pub const CLOSE_WAIT: Duration = Duration::from_millis(1500);

/// Where the connection server is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// `host[:port]`
    pub host_port: String,
    pub secure: bool,
}

impl Endpoint {
    pub fn parse(server: &str) -> Endpoint {
        let server = server.trim();
        let (scheme, rest) = match server.split_once("://") {
            Some((s, r)) => (Some(s.to_ascii_lowercase()), r),
            None => (None, server),
        };
        let host_port = rest.split('/').next().unwrap_or(rest).to_string();
        let secure = match scheme.as_deref() {
            Some("wss" | "https") => true,
            Some(_) => false,
            None => !is_this_computer(&host_port),
        };
        Endpoint { host_port, secure }
    }

    fn ws(&self, path: &str) -> String {
        format!("{}://{}{path}", if self.secure { "wss" } else { "ws" }, self.host_port)
    }

    /// Host connection: a new code, or reopen / take over `code`.
    pub fn host_url(&self, code: Option<&str>) -> String {
        match code {
            Some(c) => self.ws(&format!("/ws/host?code={c}")),
            None => self.ws("/ws/host"),
        }
    }

    /// Open rooms on our network (`lan`: our private /24, e.g. "192.168.0"; `gw`: what
    /// devices behind our router have in common, see `gachi_net::router_key`).
    pub fn nearby_url(&self, lan: Option<&str>, gw: Option<&str>) -> String {
        let query: Vec<String> = [("lan", lan), ("gw", gw)].into_iter().filter_map(|(k, v)| Some(format!("{k}={}", v?))).collect();
        if query.is_empty() {
            self.ws("/ws/nearby")
        } else {
            self.ws(&format!("/ws/nearby?{}", query.join("&")))
        }
    }

    /// Newcomer connection for `code`.
    pub fn join_url(&self, code: &str) -> String {
        self.ws(&format!("/ws/join/{code}"))
    }

    /// Plain `ws://` to another machine: passwords would be readable on the way.
    pub fn insecure_remote(&self) -> bool {
        !self.secure && !is_this_computer(&self.host_port)
    }
}

fn is_this_computer(host_port: &str) -> bool {
    let host = match host_port.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => host_port.rsplit_once(':').map_or(host_port, |(h, _)| h),
    };
    host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// TLS settings for wss://, built once: reading the Windows certificate store on every
/// connection costs noticeable time.
fn tls() -> Option<tokio_tungstenite::Connector> {
    tls_config().map(tokio_tungstenite::Connector::Rustls)
}

/// (also what the update check's HTTPS request uses)
pub fn tls_config() -> Option<std::sync::Arc<rustls::ClientConfig>> {
    static CFG: std::sync::OnceLock<Option<std::sync::Arc<rustls::ClientConfig>>> = std::sync::OnceLock::new();
    CFG.get_or_init(|| {
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_native_certs::load_native_certs().certs {
            let _ = roots.add(cert);
        }
        if roots.is_empty() {
            return None; // let tungstenite try on its own
        }
        let cfg = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
        Some(std::sync::Arc::new(cfg))
    })
    .clone()
}

/// Builds the TLS settings now (at startup), so the first connection does not wait for it.
pub fn warm_tls() {
    std::thread::spawn(|| {
        let _ = tls();
    });
}

/// Opens a WebSocket to `url`, trying once more if the first try fails quickly (a brief
/// network or DNS hiccup). Dropping the sender closes the connection.
pub async fn connect(url: &str) -> Result<(UnboundedSender<ToWorker>, UnboundedReceiver<FromWorker>), String> {
    let t = std::time::Instant::now();
    match connect_once(url).await {
        Err(e) if t.elapsed() < Duration::from_secs(3) => {
            tracing::info!("connection server: {e}; trying again");
            tokio::time::sleep(Duration::from_millis(400)).await;
            connect_once(url).await
        }
        r => r,
    }
}

async fn connect_once(url: &str) -> Result<(UnboundedSender<ToWorker>, UnboundedReceiver<FromWorker>), String> {
    // no Nagle: our messages are small and each one waits for an answer
    let connecting = tokio_tungstenite::connect_async_tls_with_config(url, None, true, tls());
    let (ws, _) = tokio::time::timeout(Duration::from_secs(8), connecting)
        .await
        .map_err(|_| "시간 초과".to_string())?
        .map_err(|e| e.to_string())?;
    let (mut sink, mut stream) = ws.split();
    let (out_tx, mut out_rx) = unbounded_channel::<ToWorker>();
    let (in_tx, in_rx) = unbounded_channel::<FromWorker>();

    tauri::async_runtime::spawn(async move {
        // say something right away. A connection on which we never send anything (a nearby
        // lookup, a join that is refused before it asks) is counted by the server's host as
        // one that broke off, however properly it is closed afterwards; one keepalive is
        // enough to make it an ordinary connection (measured against the live server)
        if sink.send(Message::Text("ping".into())).await.is_err() {
            return;
        }
        let mut keepalive = tokio::time::interval(KEEPALIVE);
        keepalive.tick().await;
        loop {
            tokio::select! {
                msg = out_rx.recv() => {
                    let Some(msg) = msg else { break };
                    let text = serde_json::to_string(&msg).expect("ToWorker serializes");
                    if sink.send(Message::Text(text)).await.is_err() {
                        return;
                    }
                }
                _ = keepalive.tick() => {
                    if sink.send(Message::Text("ping".into())).await.is_err() {
                        return;
                    }
                }
            }
        }
        let _ = sink.close().await;
    });

    tauri::async_runtime::spawn(async move {
        // a dead link (no pong for two keepalive rounds) ends the stream too
        while let Ok(Some(Ok(msg))) = tokio::time::timeout(KEEPALIVE * 2 + Duration::from_secs(5), stream.next()).await {
            let Message::Text(text) = msg else { continue };
            if text == "pong" {
                continue;
            }
            match serde_json::from_str::<FromWorker>(&text) {
                Ok(m) => {
                    if in_tx.send(m).is_err() {
                        break;
                    }
                }
                Err(e) => tracing::warn!("bad server message: {e}"),
            }
        }
        // nobody is listening any more: stay a moment longer so that the close is answered
        // (ours by the server, or the server's by us) and the connection ends as a closed
        // one, not as a broken one
        let _ = tokio::time::timeout(CLOSE_WAIT, async { while let Some(Ok(_)) = stream.next().await {} }).await;
    });

    Ok((out_tx, in_rx))
}

#[cfg(test)]
mod tests {
    use super::{connect, Endpoint};
    use gachi_proto::signal::*;

    /// One nearby lookup and one refused join against the deployed server, to watch in
    /// `wrangler tail` how they end (both should be "ok"):
    /// cargo test -p gachi live_endings -- --ignored --nocapture
    #[test]
    #[ignore = "talks to the deployed server"]
    fn live_endings() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        tauri::async_runtime::block_on(async move {
            let e = Endpoint::parse(crate::settings::DEFAULT_SERVER);
            let zeros = "0".repeat(32);
            for url in [e.nearby_url(Some("192.168.0"), Some(&zeros)), e.join_url("ZZZZZZ")] {
                let (tx, mut rx) = connect(&url).await.expect("connects");
                let answer = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await;
                println!("{:?}", answer.map(|m| m.map(|m| format!("{m:?}").chars().take(40).collect::<String>())));
                drop((tx, rx));
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        });
    }

    /// How long opening a room takes against the deployed server (prints per step):
    /// cargo test -p gachi open_timing -- --ignored --nocapture
    #[test]
    #[ignore = "talks to the deployed server"]
    fn open_timing() {
        tauri::async_runtime::block_on(async move {
            let e = Endpoint::parse(crate::settings::DEFAULT_SERVER);
            for round in 1..=3 {
                let t = std::time::Instant::now();
                let (host, mut rx) = connect(&e.host_url(None)).await.expect("connects");
                let ws = t.elapsed();
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                let t2 = std::time::Instant::now();
                let join_key = gachi_proto::to_hex(&[7; 32]);
                host.send(ToWorker::HostOpen { code: None, host_key: None, max_peers: 2, members: 1, join_key, locked: false }).unwrap();
                let Some(FromWorker::Opened { .. }) = rx.recv().await else { panic!("opened") };
                println!("round {round}: websocket {:>4} ms · code on an open connection {:>4} ms", ws.as_millis(), t2.elapsed().as_millis());
                host.send(ToWorker::Close).unwrap();
            }
        });
    }

    /// Against a running Worker (`cd worker && npx wrangler dev --port 8787`):
    /// GACHI_TEST_WORKER=127.0.0.1:8787 cargo test -p gachi worker_roundtrip -- --ignored
    #[test]
    #[ignore = "needs a local Worker"]
    fn worker_roundtrip() {
        let server = std::env::var("GACHI_TEST_WORKER").unwrap_or_else(|_| "127.0.0.1:8787".into());
        tauri::async_runtime::block_on(async move {
            let e = Endpoint::parse(&server);
            use gachi_net::joinseal;
            let host_join = joinseal::JoinKey::generate();
            let (host, mut host_rx) = connect(&e.host_url(None)).await.expect("host connects");
            let join_key = gachi_proto::to_hex(&host_join.public);
            host.send(ToWorker::HostOpen { code: None, host_key: None, max_peers: 4, members: 1, join_key, locked: true }).unwrap();
            let Some(FromWorker::Opened { code, host_key }) = host_rx.recv().await else { panic!("opened") };
            assert_eq!(code.len(), 6);
            assert_eq!(host_key.len(), 32);

            // the newcomer gets the host's key, seals its request with the password in the key
            let (joiner, mut joiner_rx) = connect(&e.join_url(&code)).await.expect("joiner connects");
            let Some(FromWorker::Host { join_key, locked: true }) = joiner_rx.recv().await else { panic!("host key") };
            let host_pub = gachi_proto::key_from_hex(&join_key).unwrap();
            let join = JoinRequest {
                seats: 0,
                profile: Profile { name: "B".into(), instrument: "건반".into(), color: None },
                candidates: vec!["192.168.0.7:5555".parse().unwrap()],
                token: 42,
                key: [9; 32],
                aes: true,
            };
            let pw = joinseal::password_key(&code, "pw");
            let (eph, sealed, answer_key) = joinseal::seal(&host_pub, Some(&pw), &serde_json::to_vec(&join).unwrap()).unwrap();
            let sj = SealedJoin { eph: gachi_proto::to_hex(&eph), pw: true, sealed: gachi_proto::to_hex(&sealed) };
            joiner.send(ToWorker::Join { join: sj.clone() }).unwrap();
            let Some(FromWorker::JoinRequest { req, join: got }) = host_rx.recv().await else { panic!("join request") };
            assert_eq!(got, sj, "the sealed request reaches the host unchanged");
            let eph = gachi_proto::key_from_hex(&got.eph).unwrap();
            let (plain, reply) = host_join.open(&eph, Some(&pw), &gachi_proto::from_hex(&got.sealed).unwrap()).expect("host opens it");
            assert_eq!(serde_json::from_slice::<JoinRequest>(&plain).unwrap(), join);

            let welcome = Welcome {
                you: 2,
                room: RoomInfo { code: code.clone(), name: "r".into(), max_peers: 4, has_password: true },
                peers: vec![PeerInfo {
                    seats: 0,
                    id: 1,
                    profile: Profile { name: "A".into(), instrument: "기타".into(), color: None },
                    is_host: true,
                    candidates: vec!["192.168.0.6:4444".parse().unwrap()],
                    token: 7,
                    key: Some([3; 32]),
                    aes: true,
                    audio: None,
                }],
            };
            let sealed = SealedWelcome { sealed: gachi_proto::to_hex(&reply.seal(&serde_json::to_vec(&welcome).unwrap())) };
            host.send(ToWorker::Admit { req, admission: Admission::Accept { welcome: sealed } }).unwrap();
            let Some(FromWorker::Welcome { welcome: got }) = joiner_rx.recv().await else { panic!("welcome") };
            let opened = answer_key.open(&gachi_proto::from_hex(&got.sealed).unwrap()).expect("only the host could seal this");
            assert_eq!(serde_json::from_slice::<Welcome>(&opened).unwrap(), welcome);
            host.send(ToWorker::Close).unwrap();
        });
    }

    #[test]
    fn picks_tls_unless_local() {
        let e = Endpoint::parse("gachi.me.workers.dev");
        assert!(e.secure);
        assert_eq!(e.host_url(None), "wss://gachi.me.workers.dev/ws/host");
        assert_eq!(e.join_url("K7MQ2X"), "wss://gachi.me.workers.dev/ws/join/K7MQ2X");
        assert_eq!(e.host_url(Some("K7MQ2X")), "wss://gachi.me.workers.dev/ws/host?code=K7MQ2X");
        assert!(!Endpoint::parse("127.0.0.1:8787").secure);
        assert!(!Endpoint::parse("localhost:8787").secure);
        assert!(!Endpoint::parse("[::1]:8787").secure);
        assert!(Endpoint::parse("192.168.0.10:8787").secure, "LAN servers need a scheme to go plain");
        let e = Endpoint::parse("ws://192.168.0.10:8787");
        assert!(!e.secure && e.insecure_remote());
        let e = Endpoint::parse("https://gachi.me.workers.dev/");
        assert_eq!((e.host_port.as_str(), e.secure), ("gachi.me.workers.dev", true));
    }
}
