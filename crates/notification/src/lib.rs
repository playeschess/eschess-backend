//! Messages to players. What a message says, and how it is delivered, are separate: the API decides *that*
//! a message is needed, a [`Mailer`] delivers it. Three mailers ship: [`LogMailer`] for development,
//! [`CapturingMailer`] for tests, and [`SmtpMailer`], which works with any provider that offers SMTP (SendGrid,
//! Mailgun, Postmark, Resend, Amazon SES, Gmail ...), so choosing a provider is configuration, not code.

mod smtp;

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::Notify;

pub use smtp::SmtpMailer;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// "Confirm your email": the one-time token proves the player can read this mailbox.
    VerifyEmail {
        to: String,
        token: String,
        ttl_minutes: i64,
    },
    /// "Reset your password": the one-time token lets whoever holds it choose a new password.
    PasswordReset {
        to: String,
        token: String,
        ttl_minutes: i64,
    },
}

impl Message {
    pub fn to(&self) -> &str {
        match self {
            Message::VerifyEmail { to, .. } | Message::PasswordReset { to, .. } => to,
        }
    }

    pub fn token(&self) -> &str {
        match self {
            Message::VerifyEmail { token, .. } | Message::PasswordReset { token, .. } => token,
        }
    }
}

/// What a player actually reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub to: String,
    pub subject: String,
    pub body: String,
}

/// Turns a message into text. `app_base_url` is where the links point (the website or the app's link domain).
pub fn render(message: &Message, app_base_url: &str) -> Rendered {
    let base = app_base_url.trim_end_matches('/');
    match message {
        Message::VerifyEmail { to, token, ttl_minutes } => Rendered {
            to: to.clone(),
            subject: "Confirm your eSchess email".into(),
            body: format!(
                "Welcome to eSchess.\n\nConfirm your email address by opening this link:\n\n{base}/verify-email?token={token}\n\n\
                 The link works once and expires in {} hours. If you did not sign up, you can ignore this email.\n",
                ttl_minutes / 60
            ),
        },
        Message::PasswordReset { to, token, ttl_minutes } => Rendered {
            to: to.clone(),
            subject: "Reset your eSchess password".into(),
            body: format!(
                "Someone asked to reset the password for this eSchess account.\n\nTo choose a new password, open this link:\n\n{base}/reset-password?token={token}\n\n\
                 The link works once and expires in {ttl_minutes} minutes. If it was not you, ignore this email: your password has not changed.\n"
            ),
        },
    }
}

#[derive(Debug)]
pub struct MailError(pub String);

impl std::fmt::Display for MailError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for MailError {}

#[async_trait]
pub trait Mailer: Send + Sync {
    async fn send(&self, message: Message) -> Result<(), MailError>;
}

/// Writes to the log instead of sending. The token is printed only when `print_tokens` is on: that is for local
/// development and must never be turned on for a real deployment, since anyone who can read the log could then
/// reset passwords.
pub struct LogMailer {
    pub print_tokens: bool,
}

#[async_trait]
impl Mailer for LogMailer {
    async fn send(&self, message: Message) -> Result<(), MailError> {
        let kind = match &message {
            Message::VerifyEmail { .. } => "verify-email",
            Message::PasswordReset { .. } => "password-reset",
        };
        if self.print_tokens {
            tracing::warn!(
                "DEV email ({kind}) to {}: token {}",
                message.to(),
                message.token()
            );
        } else {
            tracing::info!(
                "email ({kind}) for {} not sent: no mail provider configured",
                message.to()
            );
        }
        Ok(())
    }
}

/// Keeps messages in memory so tests can read what a player would have received.
#[derive(Default)]
pub struct CapturingMailer {
    sent: Mutex<Vec<Message>>,
    arrived: Notify,
}

impl CapturingMailer {
    pub fn all(&self) -> Vec<Message> {
        self.sent.lock().unwrap().clone()
    }

    /// Waits (up to two seconds) until at least `n` messages have been sent in total, then returns them all.
    /// Messages go out in a background task, so a test must wait for them rather than assume they are there.
    pub async fn wait_for(&self, n: usize) -> Vec<Message> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let notified = self.arrived.notified();
            if self.all().len() >= n || tokio::time::Instant::now() >= deadline {
                return self.all();
            }
            let _ = tokio::time::timeout_at(deadline, notified).await;
        }
    }

    /// Gives any message still being sent a moment to arrive, then returns everything. For asserting that
    /// nothing (more) was sent.
    pub async fn settle(&self) -> Vec<Message> {
        tokio::time::sleep(Duration::from_millis(150)).await;
        self.all()
    }
}

#[async_trait]
impl Mailer for CapturingMailer {
    async fn send(&self, message: Message) -> Result<(), MailError> {
        self.sent.lock().unwrap().push(message);
        self.arrived.notify_waiters();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_carry_a_link_with_the_token_and_say_how_long_it_lasts() {
        let verify = render(
            &Message::VerifyEmail {
                to: "ada@example.com".into(),
                token: "abc123".into(),
                ttl_minutes: 24 * 60,
            },
            "https://eschess.example/",
        );
        assert_eq!(verify.to, "ada@example.com");
        assert!(
            verify
                .body
                .contains("https://eschess.example/verify-email?token=abc123"),
            "{}",
            verify.body
        );
        assert!(verify.body.contains("24 hours") && verify.subject.contains("Confirm"));

        let reset = render(
            &Message::PasswordReset {
                to: "ada@example.com".into(),
                token: "xyz".into(),
                ttl_minutes: 30,
            },
            "https://eschess.example",
        );
        assert!(reset
            .body
            .contains("https://eschess.example/reset-password?token=xyz"));
        assert!(
            reset.body.contains("30 minutes") && reset.body.contains("password has not changed")
        );
    }

    #[tokio::test]
    async fn the_capturing_mailer_remembers_and_waits() {
        let mailer = std::sync::Arc::new(CapturingMailer::default());
        let sender = mailer.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            sender
                .send(Message::PasswordReset {
                    to: "a@b.test".into(),
                    token: "t".into(),
                    ttl_minutes: 30,
                })
                .await
                .unwrap();
        });
        assert_eq!(mailer.wait_for(1).await.len(), 1);
        assert_eq!(
            mailer.wait_for(5).await.len(),
            1,
            "gives up after the timeout instead of hanging"
        );
    }
}
