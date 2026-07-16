//! LSP base-protocol framing: `Content-Length: N\r\n\r\n<N bytes of UTF-8 JSON>` (the JSON-RPC 2.0
//! envelope the Language Server Protocol wraps every message in). Generic over `Write`/`BufRead` so
//! the client can be driven by a real child process OR, in tests, by an in-process fake server over
//! `std::io::pipe`.

use std::io::{self, BufRead, Write};

use serde_json::Value;

/// Frame and write one message: the `Content-Length` header (byte length of the JSON body), the
/// blank-line separator, then the UTF-8 JSON body. Flushes so the peer sees it immediately.
pub(crate) fn write_message<W: Write>(w: &mut W, msg: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(msg)?;
    write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
    w.write_all(&body)?;
    w.flush()
}

/// Read one framed message: parse headers up to the blank line (honoring `Content-Length`, ignoring
/// any others such as `Content-Type`), then read exactly that many body bytes and parse them as
/// JSON. `Ok(None)` on a clean EOF at a message boundary (the server closed its stdout).
pub(crate) fn read_message<R: BufRead>(r: &mut R) -> io::Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    let mut first_line = true;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            // EOF. At a message boundary (before any header) it is clean; part-way through the
            // headers it is a truncated frame.
            if first_line {
                return Ok(None);
            }
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF within LSP headers"));
        }
        first_line = false;
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break; // blank line ends the headers
        }
        if let Some(rest) = header.strip_prefix("Content-Length:") {
            content_length = rest.trim().parse::<usize>().ok();
        }
        // Any other header (e.g. Content-Type) is ignored.
    }
    let len = content_length.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "LSP frame missing Content-Length")
    })?;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    let value = serde_json::from_slice(&body).map_err(io::Error::other)?;
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use serde_json::json;

    use super::*;

    #[test]
    fn write_then_read_round_trips_a_message() {
        let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"x": 3}});
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        // The framed bytes carry a Content-Length header and the exact JSON body.
        let text = String::from_utf8(buf.clone()).unwrap();
        assert!(text.starts_with("Content-Length: "), "framed with a length header: {text:?}");
        let mut cursor = Cursor::new(buf);
        let read = read_message(&mut cursor).unwrap().expect("one message");
        assert_eq!(read, msg);
    }

    #[test]
    fn read_ignores_extra_headers_and_crlf() {
        // A server may send additional headers (Content-Type) before the blank line; the reader
        // keys only on Content-Length and reads exactly that many body bytes.
        let body = r#"{"jsonrpc":"2.0","id":7,"result":null}"#;
        let framed = format!(
            "Content-Length: {}\r\nContent-Type: application/vscode-jsonrpc; \
             charset=utf-8\r\n\r\n{body}",
            body.len()
        );
        let mut cursor = Cursor::new(framed.into_bytes());
        let read = read_message(&mut cursor).unwrap().expect("one message");
        assert_eq!(read, json!({"jsonrpc": "2.0", "id": 7, "result": null}));
    }

    #[test]
    fn read_two_messages_from_one_stream() {
        // Back-to-back frames read as two independent messages (the body length delimits them, so
        // the reader never over-reads into the next frame).
        let mut buf = Vec::new();
        write_message(&mut buf, &json!({"id": 1})).unwrap();
        write_message(&mut buf, &json!({"id": 2})).unwrap();
        let mut cursor = Cursor::new(buf);
        assert_eq!(read_message(&mut cursor).unwrap(), Some(json!({"id": 1})));
        assert_eq!(read_message(&mut cursor).unwrap(), Some(json!({"id": 2})));
        assert_eq!(read_message(&mut cursor).unwrap(), None, "clean EOF after the last frame");
    }

    #[test]
    fn read_returns_none_on_immediate_eof() {
        let mut cursor = Cursor::new(Vec::new());
        assert_eq!(read_message(&mut cursor).unwrap(), None);
    }
}
