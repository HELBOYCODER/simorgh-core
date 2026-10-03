//! The loopback HTTP/JSON RPC surface.
//!
//! Handwritten tokio-TCP HTTP/1.1, in the idiom of
//! `crates/zero-runtime/src/api.rs`: httparse for the request head, bounded
//! bodies, one request per connection, JSON out. No axum, no hyper — the
//! daemon should add nothing to the engine's dependency closure that the
//! engine does not already carry.
//!
//! Security posture, mirroring `api.rs` and going slightly further:
//!
//! * the listener binds loopback (startup refuses anything else), and every
//!   connection's peer address is checked again;
//! * every request must carry `Authorization: Bearer <token>` compared in
//!   constant time (no timing side channel on a shared machine);
//! * the token is generated per start-up — printed once in the
//!   `SIMORGH_READY` line and, if asked for, written to a file.
//!
//! Errors keep one shape everywhere: HTTP 200 with `{"error": "…"}` for
//! method-level failures (bad JSON, engine refused, unknown method → 404),
//! 401 for the token, 403 for a non-loopback peer, 413 for oversized bodies.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::{engine, jobs, logging};

/// Same cap as the runtime's management API.
const MAX_REQUEST: usize = 2 * 1024 * 1024;
/// How long a client may take to deliver its request.
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the event streamer polls a job's log between writes. Events are
/// batched into the log at [`crate::jobs::EVENT_BATCH_INTERVAL`] at most, so
/// this only shapes the read side.
const EVENT_POLL: Duration = Duration::from_millis(40);

pub struct State {
    pub token: String,
    pub data_dir: PathBuf,
}

pub async fn serve(listener: TcpListener, state: Arc<State>) -> std::io::Result<()> {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            // Descriptor exhaustion and aborted handshakes are transient;
            // returning would take the RPC down for good.
            Err(error) => {
                tracing::warn!(%error, "accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(error) = handle_connection(stream, peer, state).await {
                tracing::debug!(%peer, %error, "request failed");
            }
        });
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    state: Arc<State>,
) -> Result<(), String> {
    // ---- read the request head
    let deadline = tokio::time::Instant::now() + REQUEST_READ_TIMEOUT;
    let mut data = Vec::with_capacity(1024);
    let head_end;
    // Only the bytes that arrived since the last pass need scanning;
    // rescanning the whole buffer per read is quadratic in a trickled request.
    let mut scanned = 0usize;
    loop {
        let from = scanned.saturating_sub(3);
        if let Some(end) = data[from..]
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
        {
            head_end = from + end + 4;
            break;
        }
        scanned = data.len();
        if data.len() >= MAX_REQUEST {
            return write_empty(&mut stream, 413, "request too large").await;
        }
        let n = tokio::time::timeout_at(deadline, stream.read_buf(&mut data))
            .await
            .map_err(|_| "request head timed out".to_string())?
            .map_err(|error| format!("reading request: {error}"))?;
        if n == 0 {
            return Ok(());
        }
    }

    let mut headers = [httparse::EMPTY_HEADER; 48];
    let mut request = httparse::Request::new(&mut headers);
    request
        .parse(&data[..head_end])
        .map_err(|error| format!("malformed request: {error}"))?;
    let method = request.method.unwrap_or("").to_string();
    let target = request.path.unwrap_or("").to_string();
    let authorization = request
        .headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("authorization"))
        .and_then(|header| std::str::from_utf8(header.value).ok())
        .map(str::to_owned);
    let content_length = request
        .headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case("content-length"))
        .and_then(|header| {
            std::str::from_utf8(header.value)
                .ok()
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    if content_length > MAX_REQUEST - head_end {
        return write_empty(&mut stream, 413, "request too large").await;
    }
    while data.len() < head_end + content_length {
        let n = tokio::time::timeout_at(deadline, stream.read_buf(&mut data))
            .await
            .map_err(|_| "request body timed out".to_string())?
            .map_err(|error| format!("reading request body: {error}"))?;
        if n == 0 {
            return write_empty(&mut stream, 400, "truncated body").await;
        }
    }

    // ---- gate: loopback peer, then the bearer token
    if !peer.ip().is_loopback() {
        return write_empty(&mut stream, 403, "forbidden").await;
    }
    if !authorized(authorization.as_deref(), &state.token) {
        return write_json(&mut stream, 401, &json!({"error": "unauthorized"})).await;
    }

    let path = target.split(['?']).next().unwrap_or("");
    let segments: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let body = &data[head_end..head_end + content_length];
    let body: Value = if body.is_empty() {
        Value::Null
    } else {
        match serde_json::from_slice(body) {
            Ok(parsed) => parsed,
            Err(error) => {
                return write_json(
                    &mut stream,
                    200,
                    &json!({"error": format!("request body is not valid JSON: {error}")}),
                )
                .await;
            }
        }
    };

    match (method.as_str(), segments.as_slice()) {
        ("POST", ["rpc", name]) => {
            let answer = rpc_call(name, body, &state).await;
            write_json(&mut stream, 200, &answer).await
        }
        ("POST", ["job", name]) if jobs::is_job_method(name) => {
            let started = jobs::start(name, &body, &state.data_dir);
            let answer = match started {
                Ok(id) => json!({"job": id}),
                Err(error) => json!({"error": error}),
            };
            write_json(&mut stream, 200, &answer).await
        }
        ("POST", ["job", id, "cancel"]) => {
            let answer = if jobs::cancel(id) {
                json!({"ok": true})
            } else {
                json!({"error": "unknown job"})
            };
            write_json(&mut stream, 200, &answer).await
        }
        ("GET", ["job", id, "events"]) => stream_events(&mut stream, id).await,
        _ => write_json(&mut stream, 404, &json!({"error": "not found"})).await,
    }
}

/// Constant-time bearer comparison: the token is as secret as the engine's
/// whole RPC surface, and any GUI on the machine could otherwise winnow it
/// byte by byte against a non-constant `==`.
fn authorized(header: Option<&str>, token: &str) -> bool {
    let Some(presented) = header.and_then(|value| value.strip_prefix("Bearer ")) else {
        return false;
    };
    presented.as_bytes().ct_eq(token.as_bytes()).into()
}

async fn rpc_call(name: &str, body: Value, state: &State) -> Value {
    // An absent body is `{}`: several methods take no fields at all.
    let body = if body.is_null() { json!({}) } else { body };
    match name {
        "set_log_level" => match body["level"].as_str() {
            Some(text) => match logging::parse_level(text).and_then(logging::set_level) {
                Ok(()) => json!({"ok": true}),
                Err(error) => json!({"error": error}),
            },
            None => {
                json!({"error": "set_log_level needs {\"level\": \"off|error|warn|info|debug|trace\"}"})
            }
        },
        "start" => match require_config(&body) {
            Ok(config) => blocking(move || engine::start(&config)).await,
            Err(answer) => answer,
        },
        "reload" => match require_config(&body) {
            Ok(config) => blocking(move || engine::reload(&config)).await,
            Err(answer) => answer,
        },
        "stop" => blocking(engine::stop).await,
        "network_changed" => blocking(engine::network_changed).await,
        "is_running" => json!({"running": engine::is_running()}),
        // The contract's `stats()` is `String?`: null while nothing runs.
        // Over HTTP that is `{"stats": null}`; while running, the inner
        // value is byte-for-byte the contract's JSON object.
        "stats" => json!({"stats": engine::stats()}),
        "build_config" => {
            // The BuildRequest is the body itself, as in `buildConfig`.
            let assets = state.data_dir.join("assets");
            match zero_discovery::build_config_with_assets(&body, Some(assets.as_path())) {
                Ok(config) => json!({"config": config}),
                Err(error) => json!({"error": error}),
            }
        }
        "parse_links" => match body["text"].as_str() {
            Some(text) => serde_json::to_value(zero_discovery::parse_links(text))
                .unwrap_or_else(|error| json!({"error": error.to_string()})),
            None => json!({"error": "parse_links needs {\"text\": \"…\"}"}),
        },
        "verify_signature" => {
            let key = body["public_key"].as_str();
            let signed = body["body"].as_str();
            let signature = body["signature"].as_str();
            match (key, signed, signature) {
                (Some(key), Some(signed), Some(signature)) => json!({
                    "valid": zero_discovery::sign::verify_with(key, signed.as_bytes(), signature)
                }),
                _ => {
                    json!({"error": "verify_signature needs {\"public_key\": …, \"body\": …, \"signature\": …}"})
                }
            }
        }
        "built_in_public_key" => json!({"key": zero_discovery::sign::PUBLIC_KEY_HEX}),
        other => json!({"error": format!("unknown rpc method {other:?}")}),
    }
}

fn require_config(body: &Value) -> Result<Value, Value> {
    match body.get("config") {
        Some(config) if config.is_object() => Ok(config.clone()),
        _ => Err(json!({"error": "start and reload take {\"config\": <xray json>}"})),
    }
}

/// Run a blocking engine call off the async workers and fold its result into
/// the contract's `{}` (success) / `{"error": …}` answer.
async fn blocking<F>(call: F) -> Value
where
    F: FnOnce() -> Result<(), String> + Send + 'static,
{
    match tokio::task::spawn_blocking(call).await {
        Ok(Ok(())) => json!({"ok": true}),
        Ok(Err(error)) => json!({"error": error}),
        Err(_) => json!({"error": "the engine call was interrupted"}),
    }
}

/// `GET /job/<id>/events`: newline-delimited JSON, exactly the contract's
/// events, flushed as produced and closed after the terminal `done`.
async fn stream_events(stream: &mut TcpStream, id: &str) -> Result<(), String> {
    let Some(job) = jobs::lookup(id) else {
        return write_json(stream, 404, &json!({"error": "unknown job"})).await;
    };
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\n\
              Content-Type: application/x-ndjson\r\n\
              Cache-Control: no-cache\r\n\
              Connection: close\r\n\r\n",
        )
        .await
        .map_err(|error| format!("writing stream head: {error}"))?;
    let mut index = 0usize;
    loop {
        let (lines, _done) = job.snapshot(index);
        let mut finished = false;
        for line in lines {
            if is_done(&line) {
                finished = true;
            }
            stream
                .write_all(line.as_bytes())
                .await
                .map_err(|error| format!("writing event: {error}"))?;
            stream
                .write_all(b"\n")
                .await
                .map_err(|error| format!("writing event: {error}"))?;
            index += 1;
        }
        stream
            .flush()
            .await
            .map_err(|error| format!("flushing events: {error}"))?;
        if finished {
            return Ok(());
        }
        tokio::time::sleep(EVENT_POLL).await;
    }
}

fn is_done(line: &str) -> bool {
    serde_json::from_str::<Value>(line)
        .ok()
        .is_some_and(|event| event["t"] == "done")
}

async fn write_json(stream: &mut TcpStream, status: u16, body: &Value) -> Result<(), String> {
    let text = body.to_string();
    write_body(stream, status, reason(status), text.as_bytes()).await
}

async fn write_empty(stream: &mut TcpStream, status: u16, reason: &str) -> Result<(), String> {
    let body = json!({"error": reason}).to_string();
    write_body(stream, status, reason, body.as_bytes()).await
}

async fn write_body(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &[u8],
) -> Result<(), String> {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|error| format!("writing response: {error}"))?;
    stream
        .write_all(body)
        .await
        .map_err(|error| format!("writing response body: {error}"))
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Payload Too Large",
        _ => "Error",
    }
}

#[cfg(test)]
mod tests {
    use super::authorized;

    #[test]
    fn the_bearer_check_accepts_only_the_exact_token() {
        assert!(authorized(Some("Bearer secret"), "secret"));
        assert!(!authorized(Some("Bearer wrong"), "secret"));
        assert!(!authorized(Some("secret"), "secret"));
        assert!(!authorized(Some("Basic secret"), "secret"));
        assert!(!authorized(None, "secret"));
        // An empty token must not match an absent credential.
        assert!(!authorized(None, ""));
    }
}
