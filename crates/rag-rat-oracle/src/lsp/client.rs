//! The resident LSP client: process lifecycle (spawn, `initialize` handshake with position-encoding
//! negotiation, graceful `shutdown`, kill-on-drop) and synchronous JSON-RPC request/response over
//! the [`super::protocol`] framing. Single-flight by design — slice 1 issues one request at a time
//! from the maintenance-pass worker thread — so `request` reads until it sees the response for the
//! id it just sent, skipping interleaved notifications and replying `MethodNotFound` to any
//! server→client request so a server awaiting that reply can't deadlock the loop.
//!
//! The transport is injectable (`Box<dyn Write/BufRead>`): [`LspClient::spawn`] wires a child
//! process, while tests wire an in-process fake server over `std::io::pipe`. That is what makes the
//! whole client unit-testable without a real `rust-analyzer`.

use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

use serde_json::{Value, json};

use super::position::LspEncoding;
use super::protocol;

/// The JSON-RPC `MethodNotFound` code — a server's answer for a method it doesn't implement (an
/// optional request like `textDocument/moniker`), and the code we reply with to a server→client
/// request we don't handle.
const METHOD_NOT_FOUND: i64 = -32601;

/// A JSON-RPC error object from a response (`code` + `message`), so callers can distinguish a
/// `MethodNotFound` (optional-capability) from a real failure.
struct LspRpcError {
    code: i64,
    message: String,
}

/// A live language-server session. Owns the transport and (for a spawned server) the child process,
/// which it kills on drop so a crashed or abandoned pass never leaks a resident `rust-analyzer`.
pub(crate) struct LspClient {
    writer: Box<dyn Write + Send>,
    reader: Box<dyn BufRead + Send>,
    next_id: i64,
    encoding: LspEncoding,
    /// The child process for a spawned server; `None` for an injected (test) transport. Present so
    /// [`Drop`] can hard-kill it.
    child: Option<Child>,
}

impl LspClient {
    /// Spawn `program args…` as a language server, piping its stdio into the JSON-RPC transport.
    /// stderr is discarded (server diagnostics are not part of the protocol). The session starts at
    /// the LSP default encoding (UTF-16) until [`initialize`](Self::initialize) negotiates one.
    pub(crate) fn spawn(program: &str, args: &[&str]) -> io::Result<Self> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin =
            child.stdin.take().ok_or_else(|| io::Error::other("child stdin unavailable"))?;
        let stdout =
            child.stdout.take().ok_or_else(|| io::Error::other("child stdout unavailable"))?;
        Ok(Self {
            writer: Box::new(stdin),
            reader: Box::new(BufReader::new(stdout)),
            next_id: 1,
            encoding: LspEncoding::Utf16,
            child: Some(child),
        })
    }

    /// Build a client over an injected transport (no child process). The fake-server test seam.
    fn from_transport(reader: Box<dyn BufRead + Send>, writer: Box<dyn Write + Send>) -> Self {
        Self { writer, reader, next_id: 1, encoding: LspEncoding::Utf16, child: None }
    }

    /// The position encoding negotiated during [`initialize`](Self::initialize) (UTF-16 until
    /// then).
    pub(crate) fn encoding(&self) -> LspEncoding {
        self.encoding
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
    /// waiting it skips notifications AND — crucially — REPLIES to any server→client request
    /// with a `MethodNotFound` error, because a server that awaits that reply before answering
    /// us would otherwise deadlock this single-flight loop (slice 3 can add real handlers for
    /// `workspace/configuration` etc.).
    fn request_raw(
        &mut self,
        method: &str,
        params: Value,
    ) -> io::Result<Result<Value, LspRpcError>> {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        protocol::write_message(&mut self.writer, &request)?;
        loop {
            let msg = protocol::read_message(&mut self.reader)?.ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "server closed before responding")
            })?;
            // A `method`-bearing message is server→client: a notification (no id) is ignored; a
            // request (id present) MUST be answered so the server doesn't block waiting on us.
            if msg.get("method").is_some() {
                if let Some(peer_id) = msg.get("id") {
                    let reply = json!({
                        "jsonrpc": "2.0", "id": peer_id.clone(),
                        "error": {"code": METHOD_NOT_FOUND,
                                  "message": "not handled by the rag-rat live oracle"}
                    });
                    protocol::write_message(&mut self.writer, &reply)?;
                }
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
        protocol::write_message(&mut self.writer, &notification)
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
        LspClient::from_transport(
            Box::new(BufReader::new(from_server_read)),
            Box::new(to_server_write),
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
                None if msg.get("id") == Some(&json!(4242)) => Some(vec![
                    json!({"jsonrpc": "2.0", "id": pending_id.take(), "result": {"ok": true}}),
                ]),
                _ => Some(vec![]),
            }
        });
        let result = client.request("custom/thing", json!({})).unwrap();
        assert_eq!(result, json!({"ok": true}), "the original request completed after we replied");
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
        assert!(LspClient::spawn("rag-rat-no-such-lsp-xyzzy", &[]).is_err());
    }
}
