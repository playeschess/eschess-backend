//! `HttpLichess` against a stand-in server on this machine: what it sends, and how it reads the answers.

use eschess_lichess::{
    AccessToken, GameStatus, HttpLichess, Lichess, LichessError, NewChallenge, Side,
};
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

#[tokio::test]
async fn a_challenge_for_two_named_players_is_made_and_a_game_is_read_back() {
    let (base, seen) = serve(vec![
        (
            "POST /api/challenge/open",
            200,
            r#"{"id":"AbCd1234","url":"https://lichess.org/AbCd1234","urlWhite":"https://lichess.org/AbCd1234?color=white","urlBlack":"https://lichess.org/AbCd1234?color=black"}"#,
        ),
        (
            "GET /game/export/AbCd1234?moves=true&tags=false&clocks=false&evals=false",
            200,
            r#"{"id":"AbCd1234","rated":false,"variant":"standard","status":"resign","winner":"black","moves":"e2e4 e7e5 g1f3 b8c6","players":{"white":{"user":{"name":"Ada","id":"ada"}},"black":{"user":{"name":"Grace","id":"grace"}}}}"#,
        ),
        ("GET /game/export/Missing1?moves=true&tags=false&clocks=false&evals=false", 404, "{}"),
    ])
    .await;
    let l = HttpLichess::new(&base);
    let c = l
        .create_open_challenge(&NewChallenge {
            white: "ada".into(),
            black: "grace".into(),
            clock_limit_secs: 600,
            clock_increment_secs: 5,
            rated: false,
            expires_at_ms: 1_800_000_000_000,
        })
        .await
        .unwrap();
    assert_eq!(
        (c.id.as_str(), c.url_black.as_str()),
        ("AbCd1234", "https://lichess.org/AbCd1234?color=black")
    );
    let sent = seen.lock().unwrap()[0].clone();
    for want in [
        "users=ada%2Cgrace",
        "clock.limit=600",
        "clock.increment=5",
        "rated=false",
        "expiresAt=1800000000000",
        "variant=standard",
    ] {
        assert!(sent.contains(want), "{want} missing from {sent}");
    }

    let g = l.game("AbCd1234").await.unwrap().unwrap();
    assert_eq!(g.status, GameStatus::Resign);
    assert!(g.status.is_over());
    assert_eq!((g.winner, g.ply), (Some(Side::Black), 4));
    assert_eq!(
        (g.white.as_deref(), g.black.as_deref()),
        (Some("ada"), Some("grace"))
    );
    assert!(
        l.game("Missing1").await.unwrap().is_none(),
        "a game Lichess does not have is None"
    );
    assert!(matches!(
        l.game("../etc").await,
        Err(LichessError::Rejected(_))
    ));
}

#[test]
fn an_unknown_status_is_never_a_finished_game() {
    assert!(!GameStatus::Other("brandNew".into()).is_over());
    assert!(!GameStatus::Started.is_over());
    assert!(GameStatus::Aborted.is_over() && GameStatus::Cheat.is_over());
}
