//! `HttpLichess` against a stand-in server on this machine: what it sends, and how it reads the answers.

use eschess_lichess::{AccessToken, HttpLichess, Lichess, LichessError};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serves canned answers by "METHOD path" and records every request it receives (head and body as text).
async fn serve(
    answers: Vec<(&'static str, u16, &'static str)>,
) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            // Read until the headers are in and as many body bytes as Content-Length promises.
            loop {
                let n = sock.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(end) = text.find("\r\n\r\n") {
                    let want = text[..end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + want {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&buf).to_string();
            let line = text.lines().next().unwrap_or("").to_string();
            let key = line.split(' ').take(2).collect::<Vec<_>>().join(" ");
            log.lock().unwrap().push(text);
            let (status, body) = answers
                .iter()
                .find(|(k, _, _)| *k == key)
                .map(|(_, s, b)| (*s, *b))
                .unwrap_or((404, "{}"));
            let reply = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(reply.as_bytes()).await.unwrap();
        }
    });
    (base, seen)
}

#[tokio::test]
async fn the_whole_login_reads_the_account_and_revokes_the_token() {
    let (base, seen) = serve(vec![
        (
            "POST /api/token",
            200,
            r#"{"token_type":"Bearer","access_token":"lio_abc","expires_in":1}"#,
        ),
        (
            "GET /api/account",
            200,
            r#"{"id":"magnus","username":"MagNus","tosViolation":true}"#,
        ),
        ("DELETE /api/token", 204, ""),
    ])
    .await;
    let l = HttpLichess::new(&base);
    let token = l
        .exchange_code("the_code", "the_verifier", "https://app/cb", "eschess")
        .await
        .unwrap();
    let account = l.account(&token).await.unwrap();
    assert_eq!(account.id, "magnus");
    assert_eq!(account.username, "MagNus");
    assert!(account.tos_violation && !account.disabled);
    l.revoke(&token).await.unwrap();

    let seen = seen.lock().unwrap();
    assert!(seen[0].contains("grant_type=authorization_code"));
    assert!(seen[0].contains("code=the_code"));
    assert!(seen[0].contains("code_verifier=the_verifier"));
    assert!(seen[0].contains("redirect_uri=https%3A%2F%2Fapp%2Fcb"));
    assert!(seen[0].contains("client_id=eschess"));
    assert!(seen[1]
        .to_ascii_lowercase()
        .contains("authorization: bearer lio_abc"));
    assert!(seen[2].starts_with("DELETE /api/token"));
}

#[tokio::test]
async fn a_refused_code_is_a_refusal_and_a_server_error_is_not() {
    let (base, _) = serve(vec![
        (
            "POST /api/token",
            400,
            r#"{"error":"invalid_grant","error_description":"code expired"}"#,
        ),
        ("GET /api/account", 401, "{}"),
    ])
    .await;
    let l = HttpLichess::new(&base);
    assert!(matches!(
        l.exchange_code("c", "v", "r", "i").await,
        Err(LichessError::Rejected(m)) if m == "code expired"
    ));
    assert!(matches!(
        l.account(&AccessToken::new("x".into())).await,
        Err(LichessError::Rejected(_))
    ));

    let (base, _) = serve(vec![("POST /api/token", 503, "{}")]).await;
    assert!(matches!(
        HttpLichess::new(&base)
            .exchange_code("c", "v", "r", "i")
            .await,
        Err(LichessError::Unavailable(_))
    ));
}
