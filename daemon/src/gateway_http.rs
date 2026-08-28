//! A tiny HTTP client for the LinkTap gateway, because the gateway is not an HTTP/1.1 server.
//!
//! 🔴 THE FACT THAT FORCED THIS, measured against MVP's GW-02 on 2026-08-28:
//!
//! ```text
//! HTTP/1.0 200 OK
//! Server: LinkTap Gateway
//! Content-Type: text/html
//! Expires: Fri, 10 Apr 2008 14:00:00 GMT
//! Pragma: no-cache
//! ```
//!
//! No `Content-Length`. No `Transfer-Encoding`. Under HTTP/1.0 that is legal and ordinary — the
//! body ends when the server CLOSES THE CONNECTION. `curl` reads to EOF and gets the JSON.
//! reqwest/hyper speaks HTTP/1.1, will not accept a close-delimited body, and fails the request
//! outright with "connection closed before message completed" — in 0.31 s, so not a timeout.
//!
//! ⚠️ THE CONSEQUENCE WAS TOTAL, NOT PARTIAL. Every gateway call in the daemon goes through
//! `linktap::post_command`, so while it used reqwest the hub could not poll a gateway, open a
//! valve, close one, enforce a volume cutoff, or run its flood shutoff. None of it had ever worked
//! against real hardware. The tests all passed because they answer with an `axum` stub — a correct
//! HTTP/1.1 server, which is exactly what the gateway is not.
//!
//! The desktop app solved this long ago and the hub did not reuse it: `raw_linktap_post` in
//! `dashboard/src-tauri/src/lib.rs` hand-rolls the request over a TcpStream and reads to EOF,
//! commented "embedded HTTP servers that may not be fully HTTP/1.1 compliant". This is the same
//! approach, in the hub, with a timeout.
//!
//! DELIBERATELY SMALL. It speaks the one dialect this appliance speaks and nothing else: POST, a
//! JSON body, read until EOF or timeout, hand back the payload. It is not a general HTTP client and
//! must not grow into one — if something here ever needs redirects, TLS, keep-alive or chunked
//! decoding, it is not talking to a gateway any more and should not be using this.

use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// What the gateway said, or why we could not hear it.
#[derive(Debug)]
pub struct RawResponse {
    pub status: u16,
    pub body: String,
}

/// POST `body` to `http://<host>/<path>` and read the reply to EOF.
///
/// `host` may carry a port (`1.2.3.4:8080`); port 80 is assumed when it does not — the gateway's
/// own admin pages are plain port 80, and a configured host is copied verbatim from what the user
/// or discovery found.
pub async fn post_json(
    host: &str,
    path: &str,
    body: &str,
    timeout: Duration,
) -> Result<RawResponse, String> {
    let addr = if host.contains(':') { host.to_string() } else { format!("{host}:80") };
    let work = async {
        let mut stream = TcpStream::connect(&addr).await.map_err(|e| e.to_string())?;
        // `Connection: close` is not politeness, it is the FRAMING. We are asking the server to end
        // the body by closing, which is the only termination this firmware offers.
        let req = format!(
            "POST {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(req.as_bytes()).await.map_err(|e| e.to_string())?;
        stream.flush().await.map_err(|e| e.to_string())?;

        // Read to EOF. Capped so a misbehaving or hostile peer on the LAN cannot make the hub read
        // forever — a gateway reply is a few hundred bytes.
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = stream.read(&mut buf).await.map_err(|e| e.to_string())?;
            if n == 0 {
                break; // EOF — the close IS the end of the body
            }
            raw.extend_from_slice(&buf[..n]);
            if raw.len() > 256 * 1024 {
                break;
            }
        }
        Ok::<Vec<u8>, String>(raw)
    };

    let raw = tokio::time::timeout(timeout, work)
        .await
        .map_err(|_| format!("no reply from {addr} within {:?}", timeout))??;

    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = split_head_body(&text);
    let status = parse_status(&head).ok_or_else(|| "gateway sent no status line".to_string())?;
    Ok(RawResponse { status, body })
}

/// Split headers from body on the first blank line, tolerating LF-only endings.
///
/// The gateway uses CRLF, but a bare-LF appliance is exactly the sort of thing this module exists
/// to survive, so both are accepted rather than assumed.
pub fn split_head_body(text: &str) -> (String, String) {
    if let Some(i) = text.find("\r\n\r\n") {
        return (text[..i].to_string(), text[i + 4..].to_string());
    }
    if let Some(i) = text.find("\n\n") {
        return (text[..i].to_string(), text[i + 2..].to_string());
    }
    (text.to_string(), String::new())
}

/// The numeric status from a status line, for `HTTP/1.0` and `HTTP/1.1` alike.
pub fn parse_status(head: &str) -> Option<u16> {
    let line = head.lines().next()?;
    let mut parts = line.split_whitespace();
    let ver = parts.next()?;
    if !ver.starts_with("HTTP/") {
        return None;
    }
    parts.next()?.parse::<u16>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_http_1_0_status_line_parses() {
        // The whole point: 1.0 is what the gateway speaks.
        assert_eq!(parse_status("HTTP/1.0 200 OK\r\nServer: LinkTap Gateway"), Some(200));
        assert_eq!(parse_status("HTTP/1.1 404 Not Found"), Some(404));
        assert_eq!(parse_status("HTTP/1.0 500 Internal Server Error"), Some(500));
    }

    #[test]
    fn junk_is_not_a_status_line() {
        // A router's captive portal or a Shelly answering on port 80 must not read as a gateway.
        assert_eq!(parse_status("<html>hello</html>"), None);
        assert_eq!(parse_status(""), None);
        assert_eq!(parse_status("GARBAGE 200 OK"), None);
    }

    #[test]
    fn the_body_survives_the_split_with_either_line_ending() {
        let crlf = "HTTP/1.0 200 OK\r\nContent-Type: text/html\r\n\r\n<html>body</html>";
        assert_eq!(split_head_body(crlf).1, "<html>body</html>");
        let lf = "HTTP/1.0 200 OK\nContent-Type: text/html\n\n<html>body</html>";
        assert_eq!(split_head_body(lf).1, "<html>body</html>");
    }

    #[test]
    fn a_headers_only_reply_yields_an_empty_body_rather_than_swallowing_the_headers() {
        let (head, body) = split_head_body("HTTP/1.0 204 No Content");
        assert!(head.starts_with("HTTP/1.0 204"));
        assert_eq!(body, "");
    }
}

#[cfg(test)]
mod against_hardware_shaped_servers {
    //! 🔴 THE TEST CLASS WHOSE ABSENCE HID A TOTAL OUTAGE.
    //!
    //! Every other hub test answers with an `axum` stub — a correct HTTP/1.1 server. The real
    //! gateway is not one, and the difference was not cosmetic: it meant the daemon could not
    //! complete a single request against the hardware it exists to drive, while every test passed.
    //!
    //! These stubs are raw TCP and answer the way the appliance actually does: HTTP/1.0, no
    //! Content-Length, body terminated by closing the connection.
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Serve one request with `reply`, then CLOSE — which is the body's only terminator.
    async fn stub(reply: &'static str) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = l.accept().await {
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await; // drain the request
                let _ = sock.write_all(reply.as_bytes()).await;
                let _ = sock.flush().await;
                drop(sock); // the close IS the framing
            }
        });
        addr
    }

    #[tokio::test]
    async fn a_close_delimited_http_1_0_reply_is_read_in_full() {
        // Verbatim shape of MVP's GW-02, HTML wrapper and all. reqwest fails this outright.
        let addr = stub(
            "HTTP/1.0 200 OK\r\nServer: LinkTap Gateway\r\nContent-Type: text/html\r\n\r\n\
             <html><body><!--#RET-->{\"cmd\":16,\"gw_id\":\"1485A036004B1200\",\"ret\":3}</body></html>",
        ).await;
        let r = post_json(&addr, "/api.shtml", "{\"cmd\":16}", std::time::Duration::from_secs(5))
            .await
            .expect("an HTTP/1.0 close-delimited reply MUST be readable — this is the whole module");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("1485A036004B1200"), "body was truncated: {}", r.body);
    }

    #[tokio::test]
    async fn a_non_2xx_is_reported_rather_than_parsed() {
        let addr = stub("HTTP/1.0 500 Internal Server Error\r\n\r\nboom").await;
        let r = post_json(&addr, "/api.shtml", "{}", std::time::Duration::from_secs(5)).await.unwrap();
        assert_eq!(r.status, 500);
    }

    #[tokio::test]
    async fn a_server_that_accepts_and_never_answers_times_out_rather_than_hanging() {
        // 253 of every 254 addresses in a sweep are not gateways. Some ACCEPT and go quiet, which
        // is worse than a refusal: without the timeout the sweep would stall on one host forever.
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            if let Ok((sock, _)) = l.accept().await {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                drop(sock);
            }
        });
        let err = post_json(&addr, "/api.shtml", "{}", std::time::Duration::from_millis(300))
            .await
            .expect_err("a silent server must time out");
        assert!(err.contains("no reply"), "unhelpful error: {err}");
    }

    #[tokio::test]
    async fn nothing_listening_fails_fast_and_says_so() {
        let err = post_json("127.0.0.1:9", "/api.shtml", "{}", std::time::Duration::from_secs(2))
            .await
            .expect_err("a dead port must not look like a gateway");
        assert!(!err.is_empty());
    }
}
