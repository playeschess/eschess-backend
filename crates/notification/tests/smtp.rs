//! The SMTP mailer against a tiny in-process mail server, so it is tested for real without any provider.

use std::sync::{Arc, Mutex};

use eschess_notification::{Mailer, Message, SmtpMailer};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

/// Speaks just enough SMTP to accept one email, and records what it was sent.
async fn fake_smtp_server() -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let received = Arc::new(Mutex::new(Vec::new()));
    let store = received.clone();
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let store = store.clone();
            tokio::spawn(async move {
                let (read, mut write) = socket.into_split();
                let mut lines = BufReader::new(read).lines();
                let mut data = String::new();
                let mut in_data = false;
                write.write_all(b"220 fake ready\r\n").await.unwrap();
                while let Ok(Some(line)) = lines.next_line().await {
                    if in_data {
                        if line == "." {
                            in_data = false;
                            store.lock().unwrap().push(std::mem::take(&mut data));
                            write.write_all(b"250 queued\r\n").await.unwrap();
                        } else {
                            data.push_str(&line);
                            data.push('\n');
                        }
                        continue;
                    }
                    let upper = line.to_uppercase();
                    let reply: &[u8] = if upper.starts_with("EHLO") || upper.starts_with("HELO") {
                        b"250 fake\r\n"
                    } else if upper.starts_with("DATA") {
                        in_data = true;
                        b"354 go ahead\r\n"
                    } else if upper.starts_with("QUIT") {
                        let _ = write.write_all(b"221 bye\r\n").await;
                        return;
                    } else {
                        b"250 ok\r\n"
                    };
                    write.write_all(reply).await.unwrap();
                }
            });
        }
    });
    (port, received)
}

#[tokio::test]
async fn a_message_is_delivered_with_the_right_sender_recipient_subject_and_link() {
    let (port, received) = fake_smtp_server().await;
    let mailer = SmtpMailer::plain_for_tests(
        "127.0.0.1",
        port,
        "eSchess <no-reply@eschess.example>",
        "https://eschess.example",
    )
    .unwrap();
    mailer
        .send(Message::PasswordReset {
            to: "ada@example.com".into(),
            token: "reset-token-123".into(),
            ttl_minutes: 30,
        })
        .await
        .unwrap();

    let all = received.lock().unwrap().clone();
    assert_eq!(all.len(), 1);
    let mail = &all[0];
    assert!(mail.contains("no-reply@eschess.example"), "{mail}");
    assert!(mail.contains("ada@example.com"), "{mail}");
    assert!(
        mail.contains("Subject: Reset your eSchess password"),
        "{mail}"
    );
    assert!(
        mail.contains("https://eschess.example/reset-password?token=3Dreset-token-123")
            || mail.contains("https://eschess.example/reset-password?token=reset-token-123"),
        "{mail}"
    );
}

#[tokio::test]
async fn a_failure_to_send_is_reported_not_swallowed() {
    // Nothing is listening on this port.
    let mailer = SmtpMailer::plain_for_tests(
        "127.0.0.1",
        1,
        "no-reply@eschess.example",
        "https://eschess.example",
    )
    .unwrap();
    let err = mailer
        .send(Message::VerifyEmail {
            to: "ada@example.com".into(),
            token: "t".into(),
            ttl_minutes: 1440,
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("SMTP send failed"), "{err}");
}

#[test]
fn bad_configuration_is_caught_at_startup() {
    assert!(
        SmtpMailer::from_url("not a url", "no-reply@eschess.example", "https://x.example").is_err()
    );
    assert!(
        SmtpMailer::from_url("smtp://localhost:25", "not an address", "https://x.example").is_err()
    );
    assert!(SmtpMailer::from_url(
        "smtps://user:pass@smtp.example.com:465",
        "eSchess <no-reply@eschess.example>",
        "https://x.example"
    )
    .is_ok());
}
