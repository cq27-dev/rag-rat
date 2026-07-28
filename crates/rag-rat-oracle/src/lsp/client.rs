//! The resident LSP client: process lifecycle (spawn, `initialize` handshake with position-encoding
//! negotiation, graceful `shutdown`, kill-on-drop) and synchronous JSON-RPC request/response over
//! the [`super::protocol`] framing. Dedicated transport workers keep blocking pipe I/O off the
//! maintenance thread, while bounded waits prevent a wedged server from holding the repository
//! write lock forever. Requests remain single-flight: the client reads until it sees the response
//! for the id it just sent and replies to server→client requests so a server awaiting that reply
//! cannot deadlock the loop.
//!
//! The transport is injectable (`Box<dyn Write/BufRead>`): [`LspClient::spawn`] wires a child
//! process, while tests wire an in-process fake server over `std::io::pipe`. That is what makes the
//! whole client unit-testable without a real `rust-analyzer`.

use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::position::LspEncoding;
use super::protocol;

/// The JSON-RPC `MethodNotFound` code — a server's answer for a method it doesn't implement (an
/// optional request like `textDocument/moniker`), and the code we reply with to a server→client
/// request we don't handle.
const METHOD_NOT_FOUND: i64 = -32601;

/// Definition requests run while the maintenance pass owns the repository write lock. Bound every
/// request so a live but non-responsive language server cannot block all writers indefinitely.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Only responses and server requests enter this queue; ordinary notifications are discarded and
/// readiness is coalesced separately. Bounding the remainder prevents an idle server from growing
/// the client's heap without limit.
const INCOMING_QUEUE_CAPACITY: usize = 64;

type IncomingMessage = io::Result<Option<Value>>;

struct OutgoingMessage {
    message: Value,
    completed: SyncSender<io::Result<()>>,
}

/// A JSON-RPC error object from a response (`code` + `message`), so callers can distinguish a
/// `MethodNotFound` (optional-capability) from a real failure.
struct LspRpcError {
    code: i64,
    message: String,
}

/// A live language-server session. Owns the transport and (for a spawned server) the child process,
/// which it kills on drop so a crashed or abandoned pass never leaks a resident `rust-analyzer`.
pub(crate) struct LspClient {
    outgoing: SyncSender<OutgoingMessage>,
    incoming: Receiver<IncomingMessage>,
    next_id: i64,
    encoding: LspEncoding,
    request_timeout: Duration,
    server_ready: Arc<AtomicBool>,
    /// The child process for a spawned server; `None` for an injected (test) transport. Present so
    /// [`Drop`] can hard-kill it.
    child: Option<Child>,
}

impl LspClient {
    /// Spawn `program args…` from `cwd` as a language server, piping stdio into the JSON-RPC
    /// transport. stderr is discarded (server diagnostics are not part of the protocol). The
    /// session starts at the LSP default encoding (UTF-16) until
    /// [`initialize`](Self::initialize) negotiates one.
    pub(crate) fn spawn(program: &str, args: &[&str], cwd: &Path) -> io::Result<Self> {
        let mut child = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin =
            child.stdin.take().ok_or_else(|| io::Error::other("child stdin unavailable"))?;
        let stdout =
            child.stdout.take().ok_or_else(|| io::Error::other("child stdout unavailable"))?;
        let server_ready = Arc::new(AtomicBool::new(false));
        Ok(Self {
            outgoing: spawn_writer_pump(Box::new(stdin)),
            incoming: spawn_reader_pump(
                Box::new(BufReader::new(stdout)),
                Arc::clone(&server_ready),
            ),
            next_id: 1,
            encoding: LspEncoding::Utf16,
            request_timeout: REQUEST_TIMEOUT,
            server_ready,
            child: Some(child),
        })
    }

    /// Build a client over an injected transport (no child process). The fake-server test seam.
    fn from_transport(reader: Box<dyn BufRead + Send>, writer: Box<dyn Write + Send>) -> Self {
        Self::from_transport_with_timeout(reader, writer, REQUEST_TIMEOUT)
    }

    fn from_transport_with_timeout(
        reader: Box<dyn BufRead + Send>,
        writer: Box<dyn Write + Send>,
        request_timeout: Duration,
    ) -> Self {
        let server_ready = Arc::new(AtomicBool::new(false));
        Self {
            outgoing: spawn_writer_pump(writer),
            incoming: spawn_reader_pump(reader, Arc::clone(&server_ready)),
            next_id: 1,
            encoding: LspEncoding::Utf16,
            request_timeout,
            server_ready,
            child: None,
        }
    }

    /// The position encoding negotiated during [`initialize`](Self::initialize) (UTF-16 until
    /// then).
    pub(crate) fn encoding(&self) -> LspEncoding {
        self.encoding
    }

    /// Drain status notifications already emitted by the server. `true` means rust-analyzer has
    /// completed background loading (`quiescent`) and is not in an error state. Unknown status is
    /// deliberately not ready: sending definitions before the first status can turn warm-up
    /// `null`s into permanently missing evidence.
    pub(crate) fn is_server_ready(&mut self) -> io::Result<bool> {
        let deadline = Instant::now() + self.request_timeout;
        loop {
            if Instant::now() >= deadline {
                return Err(request_timeout_error("server readiness", self.request_timeout));
            }
            match self.incoming.try_recv() {
                Ok(message) => {
                    let message = message?.ok_or_else(server_closed_error)?;
                    self.handle_server_message(&message, deadline)?;
                },
                Err(TryRecvError::Empty) => {
                    return Ok(self.server_ready.load(Ordering::Relaxed));
                },
                Err(TryRecvError::Disconnected) => return Err(server_closed_error()),
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn assume_ready(&mut self) {
        self.server_ready.store(true, Ordering::Relaxed);
    }

    /// Send a request and return its `result`, mapping a JSON-RPC `error` response to an `Err`.
    pub(crate) fn request(&mut self, method: &str, params: Value) -> io::Result<Value> {
        match self.request_raw(method, params)? {
            Ok(result) => Ok(result),
            Err(err) => Err(io::Error::other(format!("LSP error {}: {}", err.code, err.message))),
        }
    }

    /// A request whose method the server may not support: a `MethodNotFound` (-32601) error becomes
    /// `Ok(None)` (the server lacks the capability) rather than failing the caller; a resolved
    /// response is `Ok(Some(result))`; any OTHER JSON-RPC error still propagates. Used for optional
    /// requests like `textDocument/moniker`.
    pub(crate) fn request_optional(
        &mut self,
        method: &str,
        params: Value,
    ) -> io::Result<Option<Value>> {
        match self.request_raw(method, params)? {
            Ok(result) => Ok(Some(result)),
            Err(err) if err.code == METHOD_NOT_FOUND => Ok(None),
            Err(err) => Err(io::Error::other(format!("LSP error {}: {}", err.code, err.message))),
        }
    }

    /// Send a request and block until its response arrives, returning the `result` (`Ok(Ok)`) or
    /// the JSON-RPC error object (`Ok(Err)`); the outer `io::Result` is the transport. While
    /// waiting it tracks notifications and replies to server→client requests. The whole operation
    /// shares one deadline, including any interleaved messages, so notification traffic cannot
    /// postpone timeout indefinitely.
    fn request_raw(
        &mut self,
        method: &str,
        params: Value,
    ) -> io::Result<Result<Value, LspRpcError>> {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let deadline = Instant::now() + self.request_timeout;
        self.send_message_until(request, deadline, method)?;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(request_timeout_error(method, self.request_timeout));
            }
            let msg = match self.incoming.recv_timeout(remaining) {
                Ok(message) => message?.ok_or_else(server_closed_error)?,
                Err(RecvTimeoutError::Timeout) => {
                    return Err(request_timeout_error(method, self.request_timeout));
                },
                Err(RecvTimeoutError::Disconnected) => return Err(server_closed_error()),
            };
            // A `method`-bearing message is server→client: a notification (no id) is ignored; a
            // request (id present) MUST be answered so the server doesn't block waiting on us.
            if msg.get("method").is_some() {
                self.handle_server_message(&msg, deadline)?;
                continue;
            }
            // A response with no `method`: ours iff the id matches (single-flight; a stray id is
            // skipped defensively).
            if msg.get("id").and_then(Value::as_i64) != Some(id) {
                continue;
            }
            if let Some(error) = msg.get("error") {
                return Ok(Err(LspRpcError {
                    code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                }));
            }
            return Ok(Ok(msg.get("result").cloned().unwrap_or(Value::Null)));
        }
    }

    /// Send a notification (no id, no response expected).
    pub(crate) fn notify(&mut self, method: &str, params: Value) -> io::Result<()> {
        let notification = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.send_message_until(notification, Instant::now() + self.request_timeout, method)
    }

    /// The `initialize`/`initialized` handshake: advertise the byte-space-friendly encodings we
    /// support, record the one the server negotiates (defaulting to the LSP protocol default,
    /// UTF-16, when the server doesn't echo one), and confirm with `initialized`.
    pub(crate) fn initialize(&mut self, root_uri: &str) -> io::Result<()> {
        let params = json!({
            "processId": null,
            "rootUri": root_uri,
            "capabilities": {
                "general": {
                    "positionEncodings": [
                        LspEncoding::Utf8.as_lsp_str(),
                        LspEncoding::Utf16.as_lsp_str(),
                        LspEncoding::Utf32.as_lsp_str(),
                    ],
                },
                "textDocument": {
                    "definition": { "dynamicRegistration": false, "linkSupport": true },
                },
                "experimental": { "serverStatusNotification": true },
            },
        });
        let result = self.request("initialize", params)?;
        // The server echoes the single encoding it chose under `capabilities.positionEncoding`;
        // absent (or unrecognized) → the LSP protocol default, UTF-16.
        self.encoding = result
            .get("capabilities")
            .and_then(|caps| caps.get("positionEncoding"))
            .and_then(Value::as_str)
            .and_then(LspEncoding::from_lsp_str)
            .unwrap_or(LspEncoding::Utf16);
        self.notify("initialized", json!({}))
    }

    /// The graceful `shutdown` request followed by the `exit` notification (the LSP teardown
    /// sequence). Kill-on-drop is the fallback if this is skipped or the server wedges.
    pub(crate) fn shutdown(&mut self) -> io::Result<()> {
        self.request("shutdown", Value::Null)?;
        self.notify("exit", Value::Null)
    }

    fn handle_server_message(&mut self, message: &Value, deadline: Instant) -> io::Result<()> {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return Ok(());
        };
        let Some(peer_id) = message.get("id") else {
            return Ok(());
        };
        let reply = match method {
            "workspace/configuration" => {
                json!({"jsonrpc": "2.0", "id": peer_id.clone(), "result": []})
            },
            "client/registerCapability" | "window/workDoneProgress/create" => {
                json!({"jsonrpc": "2.0", "id": peer_id.clone(), "result": null})
            },
            _ => json!({
                "jsonrpc": "2.0", "id": peer_id.clone(),
                "error": {"code": METHOD_NOT_FOUND,
                          "message": "not handled by the rag-rat live oracle"}
            }),
        };
        self.send_message_until(reply, deadline, method)
    }

    fn send_message_until(
        &self,
        message: Value,
        deadline: Instant,
        operation: &str,
    ) -> io::Result<()> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(request_timeout_error(operation, self.request_timeout));
        }
        let (completed, completion) = mpsc::sync_channel(1);
        self.outgoing.try_send(OutgoingMessage { message, completed }).map_err(
            |err| match err {
                TrySendError::Full(_) => io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "server stdin worker is still blocked on an earlier message",
                ),
                TrySendError::Disconnected(_) => server_stdin_closed_error(),
            },
        )?;
        match completion.recv_timeout(remaining) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) =>
                Err(request_timeout_error(operation, self.request_timeout)),
            Err(RecvTimeoutError::Disconnected) => Err(server_stdin_closed_error()),
        }
    }
}

fn spawn_writer_pump(mut writer: Box<dyn Write + Send>) -> SyncSender<OutgoingMessage> {
    let (sender, receiver) = mpsc::sync_channel::<OutgoingMessage>(1);
    thread::spawn(move || {
        while let Ok(outgoing) = receiver.recv() {
            let result = protocol::write_message(&mut writer, &outgoing.message);
            let terminal = result.is_err();
            if outgoing.completed.send(result).is_err() || terminal {
                return;
            }
        }
    });
    sender
}

fn spawn_reader_pump(
    mut reader: Box<dyn BufRead + Send>,
    server_ready: Arc<AtomicBool>,
) -> Receiver<IncomingMessage> {
    let (sender, receiver) = mpsc::sync_channel(INCOMING_QUEUE_CAPACITY);
    thread::spawn(move || {
        loop {
            let message = protocol::read_message(&mut reader);
            match message {
                Ok(Some(message)) if is_server_status(&message) => {
                    server_ready.store(status_is_ready(&message), Ordering::Relaxed);
                },
                Ok(Some(message)) if should_queue_incoming(&message) => {
                    if sender.send(Ok(Some(message))).is_err() {
                        return;
                    }
                },
                // Diagnostics, logs, and progress are irrelevant to this client.
                Ok(Some(_)) => {},
                terminal => {
                    let _ = sender.send(terminal);
                    return;
                },
            }
        }
    });
    receiver
}

fn is_server_status(message: &Value) -> bool {
    message.get("method").and_then(Value::as_str) == Some("experimental/serverStatus")
}

fn should_queue_incoming(message: &Value) -> bool {
    // Responses and server requests have ids. Ordinary notifications do not and would otherwise
    // accumulate while the watcher is idle.
    message.get("id").is_some()
}

fn status_is_ready(message: &Value) -> bool {
    let params = &message["params"];
    params.get("quiescent").and_then(Value::as_bool) == Some(true)
        && params.get("health").and_then(Value::as_str) != Some("error")
}

fn server_closed_error() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "server closed before responding")
}

fn server_stdin_closed_error() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "server stdin closed before accepting the message")
}

fn request_timeout_error(method: &str, timeout: Duration) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!("LSP request {method} timed out after {}s", timeout.as_secs_f64()),
    )
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // Hard-kill a spawned server so an abandoned/crashed pass never leaks a resident process.
        // Best-effort: a server that already exited (graceful shutdown) makes kill a no-op.
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Fake-server test harness, shared by the `client` and `resolve` unit tests.
#[cfg(test)]
pub(crate) mod test_support {
    use std::io::BufReader;
    use std::thread;
    use std::time::Duration;

    use serde_json::Value;

    use super::{LspClient, protocol};

    /// Wire an [`LspClient`] to an in-process fake server run on a thread over two
    /// `std::io::pipe`s. `handler(request)` returns `Some(messages_to_emit)` (notifications can
    /// be interleaved before a response by ordering them first), or `None` to CLOSE the
    /// connection without replying — modelling a crashed/exited server, so the client reads a
    /// real EOF rather than blocking. The server thread also exits when the client drops (its
    /// write end closes → the server reads EOF).
    pub(crate) fn client_with_server<H>(handler: H) -> LspClient
    where
        H: FnMut(&Value) -> Option<Vec<Value>> + Send + 'static,
    {
        client_with_server_timeout(handler, super::REQUEST_TIMEOUT)
    }

    pub(crate) fn client_with_server_timeout<H>(handler: H, timeout: Duration) -> LspClient
    where
        H: FnMut(&Value) -> Option<Vec<Value>> + Send + 'static,
    {
        let (to_server_read, to_server_write) = std::io::pipe().unwrap();
        let (from_server_read, from_server_write) = std::io::pipe().unwrap();
        thread::spawn(move || {
            let mut reader = BufReader::new(to_server_read);
            let mut writer = from_server_write;
            let mut handler = handler;
            while let Ok(Some(msg)) = protocol::read_message(&mut reader) {
                let Some(out) = handler(&msg) else {
                    return; // close: drop the write end so the client sees EOF
                };
                for m in out {
                    if protocol::write_message(&mut writer, &m).is_err() {
                        return;
                    }
                }
            }
        });
        LspClient::from_transport_with_timeout(
            Box::new(BufReader::new(from_server_read)),
            Box::new(to_server_write),
            timeout,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::test_support::client_with_server;
    use super::*;

    #[test]
    fn initialize_advertises_definition_link_support() {
        // Advertising DefinitionClientCapabilities.linkSupport is what makes a compliant server
        // return LocationLink (with targetSelectionRange = the identifier span) instead of a plain
        // Location whose range can span the whole declaration — which would mis-map to an enclosing
        // symbol. The selection-range preference in `parse_definition` is only reachable with this.
        let captured = Arc::new(Mutex::new(Vec::new()));
        let capture = Arc::clone(&captured);
        let mut client = client_with_server(move |msg: &Value| {
            capture.lock().unwrap().push(msg.clone());
            respond(msg, Some("utf-16"))
        });
        client.initialize("file:///repo").unwrap();
        let requests = captured.lock().unwrap();
        let init = requests
            .iter()
            .find(|m| m.get("method").and_then(Value::as_str) == Some("initialize"))
            .expect("an initialize request was sent");
        assert_eq!(
            init["params"]["capabilities"]["textDocument"]["definition"]["linkSupport"].as_bool(),
            Some(true),
        );
        assert_eq!(
            init["params"]["capabilities"]["experimental"]["serverStatusNotification"].as_bool(),
            Some(true),
        );
    }

    /// Build an `initialize` response echoing `encoding` (or none when `encoding` is `None`), and a
    /// no-op ack (stay open) for every other request/notification.
    fn respond(msg: &Value, encoding: Option<&str>) -> Option<Vec<Value>> {
        let id = msg.get("id").cloned();
        match msg.get("method").and_then(|m| m.as_str()) {
            Some("initialize") => {
                let mut caps = json!({});
                if let Some(enc) = encoding {
                    caps["positionEncoding"] = json!(enc);
                }
                Some(vec![json!({"jsonrpc": "2.0", "id": id, "result": {"capabilities": caps}})])
            },
            // Notifications (initialized/exit) have no id and need no reply; stay open.
            _ if id.is_none() => Some(vec![]),
            // Any other request → empty result.
            _ => Some(vec![json!({"jsonrpc": "2.0", "id": id, "result": null})]),
        }
    }

    #[test]
    fn initialize_negotiates_the_servers_position_encoding() {
        let mut client = client_with_server(|msg| respond(msg, Some("utf-8")));
        client.initialize("file:///repo").unwrap();
        assert_eq!(client.encoding(), LspEncoding::Utf8, "adopts the server's negotiated encoding");
    }

    #[test]
    fn initialize_defaults_to_utf16_when_the_server_omits_encoding() {
        let mut client = client_with_server(|msg| respond(msg, None));
        client.initialize("file:///repo").unwrap();
        assert_eq!(client.encoding(), LspEncoding::Utf16, "LSP protocol default when unset");
    }

    #[test]
    fn request_skips_an_interleaved_notification_before_the_response() {
        // The server emits a window/logMessage notification BEFORE the response to the same id;
        // request must skip the notification and return the response's result.
        let mut client = client_with_server(|msg| {
            let id = msg.get("id").cloned();
            Some(vec![
                json!({"jsonrpc": "2.0", "method": "window/logMessage", "params": {"message": "hi"}}),
                json!({"jsonrpc": "2.0", "id": id, "result": {"ok": true}}),
            ])
        });
        let result = client.request("custom/thing", json!({})).unwrap();
        assert_eq!(result, json!({"ok": true}));
    }

    #[test]
    fn request_surfaces_a_server_error_as_err() {
        let mut client = client_with_server(|msg| {
            let id = msg.get("id").cloned();
            Some(vec![
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": "boom"}}),
            ])
        });
        let err = client.request("custom/thing", json!({})).unwrap_err();
        assert!(err.to_string().contains("boom"), "surfaces the server's error message: {err}");
    }

    #[test]
    fn request_replies_to_a_server_initiated_request_so_it_does_not_deadlock() {
        // The server sends a server→client request (workspace/configuration) BEFORE answering ours,
        // and withholds its own response until it receives our reply to that request. If `request`
        // dropped the server request, the server would block forever (the run's terminate-after
        // would kill it). A returned result proves the client replied.
        let mut pending_id: Option<Value> = None;
        let server_reply = Arc::new(Mutex::new(None));
        let captured_reply = Arc::clone(&server_reply);
        let mut client = client_with_server(move |msg: &Value| {
            match msg.get("method").and_then(Value::as_str) {
                Some("custom/thing") => {
                    pending_id = msg.get("id").cloned();
                    Some(vec![json!({
                        "jsonrpc": "2.0", "id": 4242,
                        "method": "workspace/configuration", "params": {"items": []}
                    })])
                },
                // Our reply to the server's request (id 4242, no method) → now answer the original.
                None if msg.get("id") == Some(&json!(4242)) => {
                    *captured_reply.lock().unwrap() = Some(msg.clone());
                    Some(vec![
                        json!({"jsonrpc": "2.0", "id": pending_id.take(), "result": {"ok": true}}),
                    ])
                },
                _ => Some(vec![]),
            }
        });
        let result = client.request("custom/thing", json!({})).unwrap();
        assert_eq!(result, json!({"ok": true}), "the original request completed after we replied");
        assert_eq!(server_reply.lock().unwrap().as_ref().unwrap()["result"], json!([]));
    }

    #[test]
    fn server_status_tracks_quiescent_non_error_readiness() {
        let mut client = client_with_server(|msg: &Value| {
            let id = msg.get("id").cloned();
            match msg.get("method").and_then(Value::as_str) {
                Some("initialize") => Some(vec![
                    json!({
                        "jsonrpc": "2.0", "method": "experimental/serverStatus",
                        "params": {"health": "ok", "quiescent": true}
                    }),
                    json!({"jsonrpc": "2.0", "id": id, "result": {"capabilities": {}}}),
                ]),
                _ if id.is_none() => Some(vec![]),
                _ => Some(vec![json!({"jsonrpc": "2.0", "id": id, "result": null})]),
            }
        });
        client.initialize("file:///repo").unwrap();
        assert!(client.is_server_ready().unwrap());
    }

    #[test]
    fn request_times_out_when_the_server_stays_open_without_replying() {
        let mut client = test_support::client_with_server_timeout(
            |_msg| Some(vec![]),
            Duration::from_millis(20),
        );
        let err = client.request("custom/thing", json!({})).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(err.to_string().contains("custom/thing"));
    }

    #[test]
    fn notification_times_out_when_the_server_stops_consuming_stdin() {
        let (blocked_read, blocked_write) = std::io::pipe().unwrap();
        let mut client = LspClient::from_transport_with_timeout(
            Box::new(BufReader::new(io::empty())),
            Box::new(blocked_write),
            Duration::from_millis(20),
        );
        let err = client
            .notify("textDocument/didOpen", json!({"text": "x".repeat(8 * 1024 * 1024)}))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(err.to_string().contains("textDocument/didOpen"));
        drop(blocked_read); // Release the writer worker after proving the caller is bounded.
    }

    #[test]
    fn reader_pump_discards_an_idle_notification_flood() {
        let mut framed = Vec::new();
        for sequence in 0..INCOMING_QUEUE_CAPACITY * 2 {
            protocol::write_message(
                &mut framed,
                &json!({
                    "jsonrpc": "2.0", "method": "window/logMessage",
                    "params": {"sequence": sequence}
                }),
            )
            .unwrap();
        }
        protocol::write_message(
            &mut framed,
            &json!({"jsonrpc": "2.0", "id": 7, "result": {"ok": true}}),
        )
        .unwrap();

        let ready = Arc::new(AtomicBool::new(false));
        let incoming =
            spawn_reader_pump(Box::new(BufReader::new(std::io::Cursor::new(framed))), ready);
        assert_eq!(
            incoming.recv_timeout(Duration::from_secs(1)).unwrap().unwrap(),
            Some(json!({"jsonrpc": "2.0", "id": 7, "result": {"ok": true}})),
            "ordinary notifications must not occupy the bounded response queue"
        );
        assert!(
            incoming.recv_timeout(Duration::from_secs(1)).unwrap().unwrap().is_none(),
            "EOF follows the preserved response"
        );
    }

    #[test]
    fn request_errors_when_the_server_closes_before_responding() {
        // The server reads the request then CLOSES its connection without replying (a crash) →
        // request sees EOF, not a hang.
        let mut client = client_with_server(|_msg| None);
        let err = client.request("custom/thing", json!({})).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn spawn_of_a_missing_program_errors() {
        assert!(LspClient::spawn("rag-rat-no-such-lsp-xyzzy", &[], Path::new(".")).is_err());
    }

    #[test]
    fn spawn_applies_the_checkout_cwd() {
        let root = rag_rat_base::test_scratch::ScratchDir::new("lsp-spawn-cwd");
        assert!(
            LspClient::spawn("cargo", &["--version"], &root.path().join("missing")).is_err(),
            "a missing cwd must prevent spawn"
        );
    }
}
