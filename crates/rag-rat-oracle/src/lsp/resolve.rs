//! The per-file resolution routine: open a document's DIRTY bytes, then ask the server to resolve
//! each callee identifier via `textDocument/definition` (and, where supported,
//! `textDocument/moniker`). The output is a raw [`LiveDefinition`] per callee — the target document
//! URI, the definition RANGE in LSP position space, and any moniker — deliberately stopping short
//! of the DB: converting a target range to a byte span needs the target file's content, and mapping
//! it to a symbol id needs the index, both of which the slice-2 write path owns (it reuses
//! `join::map_definition_to_symbol`). Slice 1 is the language-server half, unit-tested against the
//! fake server.

use std::io;

use serde_json::{Value, json};

use super::client::LspClient;
use super::position::{LineIndex, LspPosition};

/// An LSP text range (a definition's span in the target document, in that document's position
/// encoding — NOT yet byte space).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LspRange {
    pub(crate) start: LspPosition,
    pub(crate) end: LspPosition,
}

/// One callee's resolved definition as the server reported it: the target document URI, the
/// definition range (LSP positions), and the moniker the server minted for the callee, if any.
/// Byte-span conversion + symbol mapping happen in slice 2 (they need the target file + the index).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveDefinition {
    pub(crate) target_uri: String,
    pub(crate) target_range: LspRange,
    pub(crate) moniker: Option<String>,
}

impl LspClient {
    /// Open a document with its DIRTY bytes (`textDocument/didOpen`) — the exact bytes whose sha
    /// the slice-2 write stamps into `file_sha`, so the resolution is against the same content.
    pub(crate) fn did_open(&mut self, uri: &str, language_id: &str, text: &str) -> io::Result<()> {
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": 1,
                    "text": text,
                }
            }),
        )
    }

    /// Ask for the definition at a position; `None` when the server returns `null` (unresolved).
    pub(crate) fn definition_at(
        &mut self,
        uri: &str,
        position: LspPosition,
    ) -> io::Result<Option<(String, LspRange)>> {
        let result = self.request(
            "textDocument/definition",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": position.line, "character": position.character },
            }),
        )?;
        Ok(parse_definition(&result))
    }

    /// Ask for the moniker at a position (`textDocument/moniker`, LSP 3.16+); `None` when the
    /// server has no moniker or does not support the request.
    pub(crate) fn moniker_at(
        &mut self,
        uri: &str,
        position: LspPosition,
    ) -> io::Result<Option<String>> {
        // `request_optional`: a server without moniker support answers `MethodNotFound`, which must
        // degrade to `None`, not fail the file. A supported-but-empty result (`null`) also parses
        // to `None`.
        let Some(result) = self.request_optional(
            "textDocument/moniker",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": position.line, "character": position.character },
            }),
        )?
        else {
            return Ok(None);
        };
        Ok(parse_moniker(&result))
    }

    /// Close a document (`textDocument/didClose`), ending the `didOpen` lifecycle.
    pub(crate) fn did_close(&mut self, uri: &str) -> io::Result<()> {
        self.notify("textDocument/didClose", json!({ "textDocument": { "uri": uri } }))
    }

    /// Resolve every callee in one dirty file as a clean open→resolve→close cycle: `didOpen` the
    /// bytes, resolve each callee, then `didClose`. The open/close pairing is what lets a RESIDENT
    /// client (reused across maintenance passes) reopen the same URI on a later pass without a
    /// double-`didOpen` — an LSP protocol violation that makes servers ignore the update and
    /// resolve stale text. The close is best-effort so it never masks the resolution result.
    ///
    /// For each callee-identifier START byte offset it issues a `textDocument/definition` (position
    /// computed in the session's negotiated encoding) and — only when the definition resolved — a
    /// `textDocument/moniker`. The result is aligned index-for-index with `callee_starts`; an
    /// unresolved callee is `None`.
    pub(crate) fn resolve_callees(
        &mut self,
        uri: &str,
        language_id: &str,
        text: &str,
        callee_starts: &[usize],
    ) -> io::Result<Vec<Option<LiveDefinition>>> {
        self.did_open(uri, language_id, text)?;
        let resolved = self.resolve_open_callees(uri, text, callee_starts);
        let _ = self.did_close(uri);
        resolved
    }

    /// [`resolve_callees`] without the per-callee `textDocument/moniker` fan-out — the slice-2
    /// write path's shape (#534). Live verdicts NEVER persist an LSP moniker (it is not
    /// interchangeable with the batch SCIP moniker string, so persisting one would trip the
    /// cross-tool conflict-drop), so the request would be pure waste against the pass's request
    /// budget. Returns `(target_uri, target_range)` per callee, aligned with `callee_starts`.
    pub(crate) fn resolve_definitions(
        &mut self,
        uri: &str,
        language_id: &str,
        text: &str,
        callee_starts: &[usize],
    ) -> io::Result<Vec<Option<(String, LspRange)>>> {
        self.did_open(uri, language_id, text)?;
        let resolved = (|| {
            let index = LineIndex::new(text.as_bytes(), self.encoding());
            let mut out = Vec::with_capacity(callee_starts.len());
            for &start in callee_starts {
                let position = index.position_at_byte(start);
                out.push(self.definition_at(uri, position)?);
            }
            Ok(out)
        })();
        let _ = self.did_close(uri);
        resolved
    }

    /// The resolution loop over an ALREADY-OPEN document (the interior of [`resolve_callees`],
    /// split out so the enclosing `didOpen`/`didClose` always pair even when a request errors).
    fn resolve_open_callees(
        &mut self,
        uri: &str,
        text: &str,
        callee_starts: &[usize],
    ) -> io::Result<Vec<Option<LiveDefinition>>> {
        let index = LineIndex::new(text.as_bytes(), self.encoding());
        let mut out = Vec::with_capacity(callee_starts.len());
        for &start in callee_starts {
            let position = index.position_at_byte(start);
            let Some((target_uri, target_range)) = self.definition_at(uri, position)? else {
                out.push(None);
                continue;
            };
            // Only spend a moniker request on a resolved callee (the per-request fan-out the plan
            // caps in slice 2's budget).
            let moniker = self.moniker_at(uri, position)?;
            out.push(Some(LiveDefinition { target_uri, target_range, moniker }));
        }
        Ok(out)
    }
}

/// Parse a `textDocument/definition` result: `Location`, `Location[]`, `LocationLink`,
/// `LocationLink[]`, or `null`. A multi-target response is authoritative only when every entry
/// names the same target; choosing the first distinct target would turn unordered alternatives
/// into a false Confirm/Upgrade/Contradict verdict.
fn parse_definition(value: &Value) -> Option<(String, LspRange)> {
    match value {
        Value::Null => None,
        Value::Array(items) => {
            let mut parsed = items.iter().map(parse_one_location);
            let first = parsed.next()??;
            parsed.all(|target| target.as_ref() == Some(&first)).then_some(first)
        },
        object => parse_one_location(object),
    }
}

/// A single `Location` (`uri`/`range`) or `LocationLink` (`targetUri` + `targetSelectionRange` /
/// `targetRange`). For a LocationLink, prefer `targetSelectionRange` (the identifier span) over
/// `targetRange` (the whole declaration, which can wrap doc comments + body): the narrower span is
/// what the slice-2 containment mapper resolves to a symbol; the wider one can hit an enclosing
/// symbol or miss entirely for a documented definition. `targetRange` is the fallback.
fn parse_one_location(value: &Value) -> Option<(String, LspRange)> {
    let uri = value.get("uri").or_else(|| value.get("targetUri")).and_then(Value::as_str)?;
    let range = value
        .get("range")
        .or_else(|| value.get("targetSelectionRange"))
        .or_else(|| value.get("targetRange"))?;
    let start = parse_position(range.get("start")?)?;
    let end = parse_position(range.get("end")?)?;
    Some((uri.to_string(), LspRange { start, end }))
}

fn parse_position(value: &Value) -> Option<LspPosition> {
    let line = u32::try_from(value.get("line")?.as_u64()?).ok()?;
    let character = u32::try_from(value.get("character")?.as_u64()?).ok()?;
    Some(LspPosition { line, character })
}

/// Parse a `textDocument/moniker` result (`Moniker[]` or `null`), returning the first moniker's
/// `scheme:identifier` (or bare `identifier` when the scheme is absent).
fn parse_moniker(value: &Value) -> Option<String> {
    let first = match value {
        Value::Array(items) => items.first()?,
        object if object.is_object() => object,
        _ => return None,
    };
    let identifier = first.get("identifier").and_then(Value::as_str)?;
    match first.get("scheme").and_then(Value::as_str) {
        Some(scheme) => Some(format!("{scheme}:{identifier}")),
        None => Some(identifier.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::lsp::client::test_support::client_with_server;
    use crate::lsp::position::LspEncoding;

    /// A canned definition the fake server returns: `(target_uri, start (line, char), end)`.
    type FakeDef = (&'static str, (u32, u32), (u32, u32));

    /// An `initialize` response advertising `encoding`, plus a definition/moniker responder driven
    /// by `def` (the target to return, or `None` for an unresolved `null`) and `moniker`.
    fn responder(
        encoding: &'static str,
        def: Option<FakeDef>,
        moniker: Option<&'static str>,
        captured: Arc<Mutex<Vec<Value>>>,
    ) -> impl FnMut(&Value) -> Option<Vec<Value>> + Send + 'static {
        move |msg: &Value| {
            captured.lock().unwrap().push(msg.clone());
            let id = msg.get("id").cloned();
            match msg.get("method").and_then(Value::as_str) {
                Some("initialize") => Some(vec![json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"capabilities": {"positionEncoding": encoding}}
                })]),
                Some("textDocument/definition") => {
                    let result = match def {
                        Some((uri, (sl, sc), (el, ec))) => json!({
                            "uri": uri,
                            "range": {"start": {"line": sl, "character": sc},
                                      "end": {"line": el, "character": ec}}
                        }),
                        None => Value::Null,
                    };
                    Some(vec![json!({"jsonrpc": "2.0", "id": id, "result": result})])
                },
                Some("textDocument/moniker") => {
                    let result = match moniker {
                        Some(ident) => json!([{"scheme": "rust-analyzer", "identifier": ident,
                                               "unique": "scheme"}]),
                        None => Value::Null,
                    };
                    Some(vec![json!({"jsonrpc": "2.0", "id": id, "result": result})])
                },
                // Notifications (initialized / didOpen) need no reply; stay open.
                _ if id.is_none() => Some(vec![]),
                _ => Some(vec![json!({"jsonrpc": "2.0", "id": id, "result": null})]),
            }
        }
    }

    #[test]
    fn resolve_callees_returns_the_definition_and_moniker_per_callee() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut client = client_with_server(responder(
            "utf-16",
            Some(("file:///defs.rs", (5, 3), (5, 9))),
            Some("cargo std foo"),
            Arc::clone(&captured),
        ));
        client.initialize("file:///repo").unwrap();
        let src = "fn caller() { foo(); }\n";
        let foo = src.find("foo").unwrap();
        let out = client.resolve_callees("file:///src.rs", "rust", src, &[foo]).unwrap();
        assert_eq!(out, vec![Some(LiveDefinition {
            target_uri: "file:///defs.rs".to_string(),
            target_range: LspRange {
                start: LspPosition { line: 5, character: 3 },
                end: LspPosition { line: 5, character: 9 },
            },
            moniker: Some("rust-analyzer:cargo std foo".to_string()),
        })]);
    }

    #[test]
    fn resolve_callees_returns_none_moniker_when_the_server_lacks_moniker_support() {
        // A server that resolves definitions but does NOT support textDocument/moniker replies to
        // the moniker request with MethodNotFound (-32601). That must degrade to `moniker: None`,
        // not fail the whole file's resolution.
        let mut client = client_with_server(|msg: &Value| {
            let id = msg.get("id").cloned();
            match msg.get("method").and_then(Value::as_str) {
                Some("initialize") => Some(vec![json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"capabilities": {"positionEncoding": "utf-16"}}
                })]),
                Some("textDocument/definition") => Some(vec![json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"uri": "file:///d.rs",
                               "range": {"start": {"line": 0, "character": 0},
                                         "end": {"line": 0, "character": 1}}}
                })]),
                Some("textDocument/moniker") => Some(vec![json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": "method not found"}
                })]),
                _ if id.is_none() => Some(vec![]),
                _ => Some(vec![json!({"jsonrpc": "2.0", "id": id, "result": null})]),
            }
        });
        client.initialize("file:///repo").unwrap();
        let out =
            client.resolve_callees("file:///s.rs", "rust", "fn a() { b(); }\n", &[9]).unwrap();
        assert_eq!(out, vec![Some(LiveDefinition {
            target_uri: "file:///d.rs".to_string(),
            target_range: LspRange {
                start: LspPosition { line: 0, character: 0 },
                end: LspPosition { line: 0, character: 1 },
            },
            moniker: None,
        })]);
    }

    #[test]
    fn resolve_callees_maps_an_unresolved_null_definition_to_none() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut client = client_with_server(responder("utf-16", None, None, Arc::clone(&captured)));
        client.initialize("file:///repo").unwrap();
        let out = client.resolve_callees("file:///s.rs", "rust", "x();\n", &[0]).unwrap();
        assert_eq!(out, vec![None]);
    }

    #[test]
    fn definition_request_position_is_in_the_negotiated_encoding() {
        // The source has an astral char before the callee. Under UTF-16 the requested position must
        // count the emoji as 2 units — proving the request side honors the negotiated encoding, not
        // raw bytes. This is the exact silent-mis-join the position invariant guards.
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut client = client_with_server(responder(
            "utf-16",
            Some(("file:///d.rs", (0, 0), (0, 1))),
            None,
            Arc::clone(&captured),
        ));
        client.initialize("file:///repo").unwrap();
        let src = "let s = \"😀\"; foo();\n";
        let foo = src.find("foo").unwrap();
        client.resolve_callees("file:///s.rs", "rust", src, &[foo]).unwrap();

        let requests = captured.lock().unwrap();
        let def_req = requests
            .iter()
            .find(|m| m.get("method").and_then(Value::as_str) == Some("textDocument/definition"))
            .expect("a definition request was sent");
        let character = def_req["params"]["position"]["character"].as_u64().unwrap();
        // `let s = "` (9) + 😀 (2 UTF-16 units) + `"; ` (3) = 14.
        assert_eq!(character, 14, "position counts the emoji as 2 UTF-16 units, not 4 bytes");
    }

    #[test]
    fn resolve_callees_opens_the_document_with_the_dirty_text() {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut client = client_with_server(responder(
            "utf-16",
            Some(("file:///d.rs", (0, 0), (0, 1))),
            None,
            Arc::clone(&captured),
        ));
        client.initialize("file:///repo").unwrap();
        let src = "fn a() { b(); }\n";
        client.resolve_callees("file:///s.rs", "rust", src, &[src.find('b').unwrap()]).unwrap();

        let requests = captured.lock().unwrap();
        let did_open = requests
            .iter()
            .find(|m| m.get("method").and_then(Value::as_str) == Some("textDocument/didOpen"))
            .expect("a didOpen notification was sent");
        assert_eq!(did_open["params"]["textDocument"]["text"].as_str(), Some(src));
        assert_eq!(did_open["params"]["textDocument"]["languageId"].as_str(), Some("rust"));
    }

    #[test]
    fn resolve_callees_closes_the_document_after_resolving() {
        // Each call is a clean open→resolve→close cycle so a resident client reused across passes
        // never double-opens a URI (an LSP protocol violation that makes servers resolve stale
        // text). A didClose must follow the resolution.
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut client = client_with_server(responder(
            "utf-16",
            Some(("file:///d.rs", (0, 0), (0, 1))),
            None,
            Arc::clone(&captured),
        ));
        client.initialize("file:///repo").unwrap();
        let src = "fn a() { b(); }\n";
        client.resolve_callees("file:///s.rs", "rust", src, &[src.find('b').unwrap()]).unwrap();
        // didClose is a fire-and-forget notification, so resolve_callees returns before the server
        // thread has necessarily recorded it. Issue a synchronous barrier request: the pipe is
        // FIFO, so when its response arrives the server has provably processed the earlier
        // didClose.
        client.request("$/barrier", Value::Null).unwrap();

        let requests = captured.lock().unwrap();
        let open_at = requests
            .iter()
            .position(|m| m.get("method").and_then(Value::as_str) == Some("textDocument/didOpen"));
        let close_at = requests
            .iter()
            .position(|m| m.get("method").and_then(Value::as_str) == Some("textDocument/didClose"));
        assert!(open_at.is_some(), "opened the document");
        assert!(
            close_at.is_some() && close_at > open_at,
            "closed the document after opening + resolving: {requests:?}"
        );
        assert_eq!(
            requests[close_at.unwrap()]["params"]["textDocument"]["uri"].as_str(),
            Some("file:///s.rs"),
        );
    }

    #[test]
    fn parse_definition_prefers_the_location_link_selection_range() {
        // rust-analyzer returns LocationLink[] when linkSupport is advertised. `targetRange` spans
        // the whole declaration (here lines 0–5, incl. a doc comment + body);
        // `targetSelectionRange` is the identifier span. The narrower selection range is
        // what maps to a symbol, so it wins.
        let value = json!([{
            "targetUri": "file:///t.rs",
            "targetRange": {"start": {"line": 0, "character": 0}, "end": {"line": 5, "character": 1}},
            "targetSelectionRange": {"start": {"line": 2, "character": 3}, "end": {"line": 2, "character": 9}}
        }]);
        assert_eq!(
            parse_definition(&value),
            Some(("file:///t.rs".to_string(), LspRange {
                start: LspPosition { line: 2, character: 3 },
                end: LspPosition { line: 2, character: 9 },
            }))
        );
    }

    #[test]
    fn parse_definition_rejects_distinct_multi_target_responses() {
        let range = |line| {
            json!({"start": {"line": line, "character": 0},
                   "end": {"line": line, "character": 1}})
        };
        let ambiguous = json!([
            {"uri": "file:///a.rs", "range": range(0)},
            {"uri": "file:///b.rs", "range": range(1)}
        ]);
        assert_eq!(parse_definition(&ambiguous), None);

        let duplicate = json!([
            {"uri": "file:///a.rs", "range": range(0)},
            {"uri": "file:///a.rs", "range": range(0)}
        ]);
        assert!(parse_definition(&duplicate).is_some(), "duplicate targets are unambiguous");
    }

    #[test]
    fn resolve_definitions_skips_the_moniker_fan_out() {
        // The slice-2 write path never persists an LSP moniker, so the definitions-only variant
        // must not spend a moniker request per callee — assert none leaves the client.
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut client = client_with_server(responder(
            "utf-16",
            Some(("file:///defs.rs", (5, 3), (5, 9))),
            Some("cargo std foo"),
            Arc::clone(&captured),
        ));
        client.initialize("file:///repo").unwrap();
        let src = "fn caller() { foo(); }\n";
        let foo = src.find("foo").unwrap();
        let out = client.resolve_definitions("file:///src.rs", "rust", src, &[foo]).unwrap();
        assert_eq!(out, vec![Some(("file:///defs.rs".to_string(), LspRange {
            start: LspPosition { line: 5, character: 3 },
            end: LspPosition { line: 5, character: 9 },
        },))]);
        let requests = captured.lock().unwrap();
        assert!(
            !requests
                .iter()
                .any(|m| m.get("method").and_then(Value::as_str) == Some("textDocument/moniker")),
            "no moniker request may be issued: {requests:?}"
        );
    }

    #[test]
    fn encoding_is_reachable() {
        // Guard: the resolve routine must use the client's negotiated encoding (compile-time link).
        assert_eq!(LspEncoding::Utf16.as_lsp_str(), "utf-16");
    }
}
