//! SSAP over `wss://<host>:3001`: connect, register (with/without key), request/response by id.
//!
//! Message formats follow `lg-webos-client` and alga. The manifest is unsigned, as in
//! alga: this TV blacklists the signed one from lg-webos-client/lgtv2/pywebostv.
//!
//! - register: `{"type":"register","id":"register_0","payload":{"pairingType":"PROMPT","manifest":{...},"client-key":"..."}}`
//! - prompt shown: `{"type":"response","id":"register_0","payload":{"pairingType":"PROMPT","returnValue":true}}`
//! - paired: `{"type":"registered","id":"register_0","payload":{"client-key":"..."}}`
//! - request: `{"type":"request","id":"1","uri":"ssap://...","payload":{...}}`
//! - errors: `{"type":"error","id":"1","error":"401 insufficient permissions","payload":{}}`,
//!   or a `response` whose payload has `"returnValue":false` (with `errorText`).

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use crate::config::Config;
use crate::tls;

/// SSAP's TLS websocket port.
pub const PORT: u16 = 3001;
/// Default timeout for [`Client::request`].
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

const REGISTER_ID: &str = "register_0";

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A connected (and possibly registered) SSAP session.
pub struct Client {
    ws: Ws,
    next_id: u64,
}

impl Client {
    /// Open `wss://<host>:3001` with the TLS mode from `cfg`, failing after `timeout`.
    pub async fn connect(cfg: &Config, timeout: Duration) -> Result<Self> {
        let url = format!("wss://{}:{PORT}", cfg.host);
        let tls = tls::client_config(cfg.tls)?;
        let fut = tokio_tungstenite::connect_async_tls_with_config(
            url.as_str(),
            None,
            true,
            Some(Connector::Rustls(tls)),
        );
        let (ws, _response) = tokio::time::timeout(timeout, fut)
            .await
            .map_err(|_| ConnectTimeout(timeout))
            .with_context(|| format!("connecting to {url}"))?
            .with_context(|| format!("connecting to {url}"))?;
        tracing::debug!("connected to {url}");
        Ok(Self { ws, next_id: 1 })
    }

    /// Send `register` (with `client_key` if given); wait up to `timeout` for `registered`.
    /// Returns the client key the TV answered with.
    ///
    /// If the TV shows the pairing prompt, this logs it and keeps waiting, so `timeout`
    /// should leave the user time to accept (e.g. 60s for `pair`).
    pub async fn register(
        &mut self,
        client_key: Option<&str>,
        timeout: Duration,
    ) -> Result<String> {
        self.send(&register_message(REGISTER_ID, client_key))
            .await?;
        let wait = async {
            loop {
                let text = self.recv_text().await?;
                match parse_message(&text)? {
                    Incoming::Registered { client_key, .. } => return Ok(client_key),
                    Incoming::Prompt { .. } => {
                        if client_key.is_some() {
                            tracing::warn!(
                                "the TV did not accept the stored client key and is showing \
                                 the pairing prompt; accept it on the TV (or run `lgtv-wake pair --force`)"
                            );
                        } else {
                            tracing::info!(
                                "pairing prompt shown on the TV, waiting for it to be accepted"
                            );
                        }
                    }
                    Incoming::Error { message, .. } => bail!("TV refused registration: {message}"),
                    other => tracing::debug!("ignoring message while registering: {other:?}"),
                }
            }
        };
        tokio::time::timeout(timeout, wait)
            .await
            .map_err(|_| anyhow!("TV did not complete registration within {timeout:?}"))?
    }

    /// Send a `request` for `uri` with `payload`; return the matching response payload.
    /// Waits up to [`REQUEST_TIMEOUT`].
    pub async fn request(&mut self, uri: &str, payload: Value) -> Result<Value> {
        self.request_with_timeout(uri, payload, REQUEST_TIMEOUT)
            .await
    }

    /// Like [`Client::request`] with an explicit timeout.
    pub async fn request_with_timeout(
        &mut self,
        uri: &str,
        payload: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.next_id.to_string();
        self.next_id += 1;
        self.send(&request_message(&id, uri, payload)).await?;
        let wait = async {
            loop {
                let text = self.recv_text().await?;
                let msg = parse_message(&text)?;
                if msg.id() != Some(id.as_str()) {
                    tracing::debug!("ignoring unrelated message: {msg:?}");
                    continue;
                }
                return response_result(msg);
            }
        };
        tokio::time::timeout(timeout, wait)
            .await
            .map_err(|_| anyhow!("no response from the TV within {timeout:?}"))?
            .with_context(|| format!("request {uri}"))
    }

    /// Close the websocket politely (errors are ignored).
    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }

    async fn send(&mut self, msg: &Value) -> Result<()> {
        tracing::trace!("-> {msg}");
        self.ws
            .send(Message::text(msg.to_string()))
            .await
            .context("sending to the TV")
    }

    /// Next text frame; pings are answered by tungstenite, binary frames skipped.
    async fn recv_text(&mut self) -> Result<String> {
        loop {
            let msg = self
                .ws
                .next()
                .await
                .ok_or_else(|| anyhow!("the TV closed the connection"))?
                .context("reading from the TV")?;
            match msg {
                Message::Text(text) => {
                    tracing::trace!("<- {}", text.as_str());
                    return Ok(text.as_str().to_owned());
                }
                Message::Close(frame) => {
                    return Err(Closed(frame.map(|f| f.reason.as_str().to_owned())).into());
                }
                _ => continue,
            }
        }
    }
}

/// The TV closed the websocket, with the close frame's reason if it sent one. While it
/// shuts down it accepts connections for a few seconds, then closes them with "Try Again
/// Later (EWS)".
#[derive(Debug, Clone)]
pub struct Closed(pub Option<String>);

impl std::fmt::Display for Closed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Some(reason) => write!(f, "the TV closed the connection ({reason})"),
            None => f.write_str("the TV closed the connection"),
        }
    }
}

impl std::error::Error for Closed {}

/// Whether the TV turned the connection away as busy ("Try Again Later"), as it does
/// for a few seconds while shutting down.
pub fn is_busy(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<Closed>()
            .and_then(|c| c.0.as_deref())
            .is_some_and(|reason| reason.contains("Try Again Later"))
    })
}

/// [`Client::connect`] got no answer within the timeout.
#[derive(Debug, Clone, Copy)]
pub struct ConnectTimeout(pub Duration);

impl std::fmt::Display for ConnectTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no answer within {:?}", self.0)
    }
}

impl std::error::Error for ConnectTimeout {}

/// Whether a [`Client::connect`] error means "nothing is listening" (TV off or in
/// standby: timeout, refused, unreachable, reset) rather than a real failure such as a
/// TLS certificate mismatch.
pub fn is_unreachable(err: &anyhow::Error) -> bool {
    use std::io::ErrorKind::*;
    use tokio_tungstenite::tungstenite::Error as WsError;
    let network_kind = |e: &std::io::Error| {
        matches!(
            e.kind(),
            ConnectionRefused
                | ConnectionReset
                | ConnectionAborted
                | TimedOut
                | HostUnreachable
                | NetworkUnreachable
                | NetworkDown
                | NotConnected
                | AddrNotAvailable
                | UnexpectedEof
        )
    };
    err.chain().any(|cause| {
        if cause.is::<ConnectTimeout>() {
            return true;
        }
        match cause.downcast_ref::<WsError>() {
            Some(WsError::Io(e)) => network_kind(e),
            Some(WsError::ConnectionClosed | WsError::AlreadyClosed) => true,
            Some(_) => false,
            None => cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(network_kind),
        }
    })
}

/// Build the `register` message with an unsigned manifest.
pub fn register_message(id: &str, client_key: Option<&str>) -> Value {
    let mut payload = json!({
        "forcePairing": false,
        "pairingType": "PROMPT",
        "manifest": manifest(),
    });
    if let Some(key) = client_key {
        payload["client-key"] = Value::from(key);
    }
    json!({ "type": "register", "id": id, "payload": payload })
}

/// Build a `request` message. A `null` payload is sent as `{}`.
pub fn request_message(id: &str, uri: &str, payload: Value) -> Value {
    let payload = if payload.is_null() {
        json!({})
    } else {
        payload
    };
    json!({ "type": "request", "id": id, "uri": uri, "payload": payload })
}

/// A message from the TV, classified.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// `registered` with a client key: pairing done.
    Registered {
        id: Option<String>,
        client_key: String,
    },
    /// `response` with `pairingType`: the pairing prompt is on screen.
    Prompt { id: Option<String> },
    /// Any other `response`.
    Response { id: Option<String>, payload: Value },
    /// `error`, with its message.
    Error { id: Option<String>, message: String },
    /// Anything else (e.g. other message types).
    Other { kind: String, id: Option<String> },
}

impl Incoming {
    pub fn id(&self) -> Option<&str> {
        match self {
            Incoming::Registered { id, .. }
            | Incoming::Prompt { id }
            | Incoming::Response { id, .. }
            | Incoming::Error { id, .. }
            | Incoming::Other { id, .. } => id.as_deref(),
        }
    }
}

/// Parse and classify a text frame from the TV.
pub fn parse_message(text: &str) -> Result<Incoming> {
    let v: Value =
        serde_json::from_str(text).with_context(|| format!("invalid JSON from the TV: {text}"))?;
    let kind = v["type"].as_str().unwrap_or_default();
    let id = match &v["id"] {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    let payload = v.get("payload").cloned().unwrap_or(Value::Null);
    Ok(match kind {
        "registered" => {
            let client_key = payload["client-key"]
                .as_str()
                .ok_or_else(|| anyhow!("`registered` without a client key: {text}"))?
                .to_owned();
            Incoming::Registered { id, client_key }
        }
        "response" if payload.get("pairingType").is_some() => Incoming::Prompt { id },
        "response" => Incoming::Response { id, payload },
        "error" => {
            let message = v["error"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| error_text(&payload))
                .unwrap_or_else(|| text.to_owned());
            Incoming::Error { id, message }
        }
        _ => Incoming::Other {
            kind: kind.to_owned(),
            id,
        },
    })
}

/// Turn the message answering a request into its payload, or an error for
/// `error` messages and `returnValue: false`.
pub fn response_result(msg: Incoming) -> Result<Value> {
    match msg {
        Incoming::Response { payload, .. } => {
            if payload.get("returnValue") == Some(&Value::Bool(false)) {
                let detail = error_text(&payload).unwrap_or_else(|| payload.to_string());
                bail!("TV returned an error: {detail}");
            }
            Ok(payload)
        }
        Incoming::Error { message, .. } => bail!("TV returned an error: {message}"),
        other => bail!("unexpected reply from the TV: {other:?}"),
    }
}

/// `errorText` (with `errorCode` if present) from a failed payload.
fn error_text(payload: &Value) -> Option<String> {
    let text = payload["errorText"].as_str()?;
    Some(match &payload["errorCode"] {
        Value::Null => text.to_owned(),
        code => format!("{text} (code {code})"),
    })
}

/// An unsigned manifest, as alga sends. Recent webOS firmware rejects the old signed
/// manifest from LG's remote app (lg-webos-client, lgtv2, pywebostv) with `403 Pairing
/// rejected: blacklisted certificate detected`. The permissions cover everything
/// `lgtv-wake` needs.
fn manifest() -> Value {
    json!({
        "manifestVersion": 1,
        "appVersion": "1.1",
        "permissions": [
            "LAUNCH",
            "LAUNCH_WEBAPP",
            "APP_TO_APP",
            "CLOSE",
            "TEST_OPEN",
            "TEST_PROTECTED",
            "CONTROL_AUDIO",
            "CONTROL_DISPLAY",
            "CONTROL_INPUT_JOYSTICK",
            "CONTROL_INPUT_MEDIA_RECORDING",
            "CONTROL_INPUT_MEDIA_PLAYBACK",
            "CONTROL_INPUT_TV",
            "CONTROL_POWER",
            "READ_APP_STATUS",
            "READ_CURRENT_CHANNEL",
            "READ_INPUT_DEVICE_LIST",
            "READ_NETWORK_STATE",
            "READ_RUNNING_APPS",
            "READ_TV_CHANNEL_LIST",
            "WRITE_NOTIFICATION_TOAST",
            "READ_POWER_STATE",
            "READ_COUNTRY_INFO"
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_without_key() {
        let m = register_message("register_0", None);
        assert_eq!(m["type"], "register");
        assert_eq!(m["id"], "register_0");
        assert_eq!(m["payload"]["pairingType"], "PROMPT");
        assert_eq!(m["payload"]["forcePairing"], false);
        assert!(m["payload"].get("client-key").is_none());
        let manifest = &m["payload"]["manifest"];
        assert_eq!(manifest["manifestVersion"], 1);
        assert!(manifest.get("signed").is_none());
        assert!(manifest.get("signatures").is_none());
        let perms: Vec<&str> = manifest["permissions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap())
            .collect();
        for needed in [
            "CONTROL_POWER",
            "CONTROL_INPUT_TV",
            "READ_RUNNING_APPS",
            "READ_POWER_STATE",
        ] {
            assert!(perms.contains(&needed), "missing {needed}");
        }
    }

    #[test]
    fn register_with_key_roundtrips_as_json() {
        let m = register_message("register_0", Some("abc123"));
        let text = m.to_string();
        let back: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(back["payload"]["client-key"], "abc123");
        assert_eq!(back, m);
    }

    #[test]
    fn request_serialization() {
        let m = request_message("3", "ssap://tv/switchInput", json!({"inputId": "HDMI_1"}));
        assert_eq!(
            m,
            json!({
                "type": "request",
                "id": "3",
                "uri": "ssap://tv/switchInput",
                "payload": {"inputId": "HDMI_1"}
            })
        );
        let m = request_message("4", "ssap://system/turnOff", Value::Null);
        assert_eq!(m["payload"], json!({}));
    }

    #[test]
    fn parse_prompt() {
        let msg = parse_message(
            r#"{"type":"response","id":"register_0","payload":{"pairingType":"PROMPT","returnValue":true}}"#,
        )
        .unwrap();
        assert_eq!(
            msg,
            Incoming::Prompt {
                id: Some("register_0".into())
            }
        );
    }

    #[test]
    fn parse_registered() {
        let msg = parse_message(
            r#"{"type":"registered","id":"register_0","payload":{"client-key":"0123abcd"}}"#,
        )
        .unwrap();
        assert_eq!(
            msg,
            Incoming::Registered {
                id: Some("register_0".into()),
                client_key: "0123abcd".into()
            }
        );
        assert!(parse_message(r#"{"type":"registered","id":"register_0","payload":{}}"#).is_err());
    }

    #[test]
    fn parse_error() {
        let msg = parse_message(
            r#"{"type":"error","id":"register_0","error":"403 User rejected pairing","payload":{}}"#,
        )
        .unwrap();
        assert_eq!(
            msg,
            Incoming::Error {
                id: Some("register_0".into()),
                message: "403 User rejected pairing".into()
            }
        );
        let err = response_result(msg).unwrap_err().to_string();
        assert!(err.contains("403 User rejected pairing"), "{err}");
    }

    #[test]
    fn parse_response_ok() {
        let msg = parse_message(
            r#"{"type":"response","id":"2","payload":{"returnValue":true,"state":"Active"}}"#,
        )
        .unwrap();
        assert_eq!(msg.id(), Some("2"));
        let payload = response_result(msg).unwrap();
        assert_eq!(payload["state"], "Active");
    }

    #[test]
    fn parse_response_return_value_false() {
        let msg = parse_message(
            r#"{"type":"response","id":"5","payload":{"returnValue":false,"errorCode":-1000,"errorText":"Invalid input"}}"#,
        )
        .unwrap();
        let err = response_result(msg).unwrap_err().to_string();
        assert!(err.contains("Invalid input"), "{err}");
        assert!(err.contains("-1000"), "{err}");
    }

    #[test]
    fn parse_numeric_id_and_other_types() {
        let msg = parse_message(r#"{"type":"hello","id":7,"payload":{}}"#).unwrap();
        assert_eq!(
            msg,
            Incoming::Other {
                kind: "hello".into(),
                id: Some("7".into())
            }
        );
        assert!(response_result(msg).is_err());
        assert!(parse_message("not json").is_err());
    }

    #[test]
    fn unreachable_classification() {
        use std::io;
        use tokio_tungstenite::tungstenite::Error as WsError;

        let timeout = anyhow::Error::new(ConnectTimeout(Duration::from_secs(2))).context("x");
        assert!(is_unreachable(&timeout));
        let refused = anyhow::Error::new(WsError::Io(io::ErrorKind::ConnectionRefused.into()))
            .context("connecting");
        assert!(is_unreachable(&refused));
        let tls = anyhow::Error::new(WsError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            rustls::Error::General(crate::tls::PIN_MISMATCH.into()),
        )));
        assert!(!is_unreachable(&tls));
        assert!(!is_unreachable(&anyhow!("something else")));
    }

    #[test]
    fn busy_classification() {
        let busy =
            anyhow::Error::new(Closed(Some("Try Again Later (EWS)".into()))).context("registering");
        assert!(is_busy(&busy));
        assert!(!is_unreachable(&busy));
        assert!(!is_busy(&anyhow::Error::new(Closed(None))));
        assert!(!is_busy(&anyhow::Error::new(Closed(Some("bye".into())))));
        assert!(!is_busy(&anyhow!("Try Again Later")));
    }

    /// Nothing listening on the port: connect fails fast and counts as unreachable.
    #[tokio::test]
    async fn connect_refused_is_unreachable() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        // Client::connect always uses PORT, so drive tungstenite directly for the local port.
        let tls = crate::tls::client_config(crate::config::TlsMode::Pinned).unwrap();
        let err = tokio_tungstenite::connect_async_tls_with_config(
            format!("wss://127.0.0.1:{port}"),
            None,
            true,
            Some(Connector::Rustls(tls)),
        )
        .await
        .expect_err("nothing is listening");
        assert!(is_unreachable(
            &anyhow::Error::new(err).context("connecting")
        ));
    }

    /// TLS + websocket handshake with the real TV and the pinned cert; no register,
    /// so no prompt. `cargo test -- --ignored live_`
    #[tokio::test]
    #[ignore = "needs the TV on the network"]
    async fn live_connect_pinned() {
        let cfg = Config::parse(crate::config::TEMPLATE).unwrap();
        let client = Client::connect(&cfg, Duration::from_secs(5)).await.unwrap();
        client.close().await;
    }
}
