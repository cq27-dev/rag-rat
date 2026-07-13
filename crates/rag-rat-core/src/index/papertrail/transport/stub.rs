//! Test-only scripted HTTP/1.1 stub for the transport tests, following the in-process stub
//! discipline from the embedding backends: accepted sockets are forced BLOCKING (macOS/BSD
//! `accept()` inherits the listener's `O_NONBLOCK`, which turns read timeouts into truncation)
//! and every request is fully drained before the response is written (closing with unread bytes
//! in the recv buffer is an abortive RST on Windows, surfacing as a transport error).

use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

/// One scripted response. The stub serves them in order, one connection each
/// (`Connection: close`).
pub(crate) struct StubResponse {
    pub status: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl StubResponse {
    pub(crate) fn ok(body: &str) -> Self {
        Self::status("200 OK", body)
    }

    pub(crate) fn status(status: &'static str, body: &str) -> Self {
        Self { status, headers: Vec::new(), body: body.to_string() }
    }

    /// A `200 OK` carrying GitHub-style quota headers.
    pub(crate) fn ok_with_quota(
        body: &str,
        limit: i64,
        remaining: i64,
        reset_epoch_s: i64,
    ) -> Self {
        Self {
            status: "200 OK",
            headers: vec![
                ("x-ratelimit-limit".to_string(), limit.to_string()),
                ("x-ratelimit-remaining".to_string(), remaining.to_string()),
                ("x-ratelimit-reset".to_string(), reset_epoch_s.to_string()),
            ],
            body: body.to_string(),
        }
    }
}

/// Spawn a stub on `127.0.0.1:0` that serves the scripted responses in order — one accepted
/// connection per response — and returns the base URL plus a join handle yielding the captured
/// request HEADS (request line + headers) in arrival order, so tests can assert the exact page
/// sequence. Accepts poll with a deadline so a client-side bug can't hang the join forever.
pub(crate) fn spawn_script_stub(
    responses: Vec<StubResponse>,
) -> (String, thread::JoinHandle<Vec<String>>) {
    spawn_script_stub_with_timeout(responses, Duration::from_secs(10))
}

fn spawn_script_stub_with_timeout(
    responses: Vec<StubResponse>,
    accept_timeout: Duration,
) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.set_nonblocking(true).expect("nonblocking listener");
    let port = listener.local_addr().unwrap().port();
    let base = format!("http://127.0.0.1:{port}");
    let responses = responses
        .into_iter()
        .map(|mut response| {
            response.body = response.body.replace("{BASE}", &base);
            for (_, value) in &mut response.headers {
                *value = value.replace("{BASE}", &base);
            }
            response
        })
        .collect::<Vec<_>>();
    let handle = thread::spawn(move || {
        let mut captured = Vec::new();
        for response in responses {
            let deadline = Instant::now() + accept_timeout;
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                    Err(_) => return captured,
                }
            };
            if let Some(head) = read_full_request(&mut stream) {
                captured.push(head);
            }
            let payload = format!(
                "HTTP/1.1 {}\r\n{}Content-Type: application/json\r\nContent-Length: \
                 {}\r\nConnection: close\r\n\r\n{}",
                response.status,
                response
                    .headers
                    .iter()
                    .map(|(name, value)| format!("{name}: {value}\r\n"))
                    .collect::<String>(),
                response.body.len(),
                response.body
            );
            let _ = stream.write_all(payload.as_bytes());
            let _ = stream.flush();
        }
        captured
    });
    (base, handle)
}

/// Read the request head + its full `Content-Length` body (drained, discarded), returning the
/// head. The blocking flip is the macOS/BSD fix; the full drain is the Windows fix.
fn read_full_request(stream: &mut TcpStream) -> Option<String> {
    stream.set_nonblocking(false).ok();
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        if let Some(end) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&raw[..end]).to_string();
            let content_len = head
                .to_ascii_lowercase()
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let body_start = end + 4;
            while raw.len() < body_start + content_len {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => raw.extend_from_slice(&buf[..n]),
                }
            }
            return Some(head);
        }
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return None,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Shutdown;

    use super::*;

    fn connected_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind pair listener");
        let client =
            TcpStream::connect(listener.local_addr().expect("pair address")).expect("connect pair");
        let (server, _) = listener.accept().expect("accept pair");
        (client, server)
    }

    #[test]
    fn scripted_stub_stops_waiting_after_its_accept_deadline() {
        let (_, handle) =
            spawn_script_stub_with_timeout(vec![StubResponse::ok("unused")], Duration::ZERO);
        assert!(handle.join().expect("stub thread").is_empty());
    }

    #[test]
    fn request_reader_returns_none_when_the_peer_closes_before_a_head() {
        let (client, mut server) = connected_pair();
        client.shutdown(Shutdown::Write).expect("close writer");
        assert_eq!(read_full_request(&mut server), None);
    }

    #[test]
    fn request_reader_keeps_the_head_when_a_declared_body_is_truncated() {
        let (mut client, mut server) = connected_pair();
        client.set_nodelay(true).expect("disable buffering");
        let writer = thread::spawn(move || {
            client
                .write_all(b"POST /items HTTP/1.1\r\nContent-Length: 10\r\n\r\n")
                .expect("write request head");
            thread::sleep(Duration::from_millis(20));
            client.write_all(b"short").expect("write partial body");
            client.shutdown(Shutdown::Write).expect("close writer");
        });
        assert_eq!(
            read_full_request(&mut server).as_deref(),
            Some("POST /items HTTP/1.1\r\nContent-Length: 10")
        );
        writer.join().expect("writer thread");
    }
}
