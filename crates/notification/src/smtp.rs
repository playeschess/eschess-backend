use async_trait::async_trait;
use lettre::message::Mailbox;
use lettre::transport::smtp::AsyncSmtpTransport;
use lettre::{AsyncTransport, Message as Email, Tokio1Executor};

use crate::{render, MailError, Mailer, Message};

/// Sends through any SMTP server. In production `url` is something like `smtps://username:password@smtp.provider.com:465`
/// (it contains a password, so it comes from the environment and is never logged).
pub struct SmtpMailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
    app_base_url: String,
}

impl SmtpMailer {
    pub fn from_url(url: &str, from: &str, app_base_url: &str) -> Result<Self, MailError> {
        let transport = AsyncSmtpTransport::<Tokio1Executor>::from_url(url)
            .map_err(|e| MailError(format!("invalid SMTP_URL: {e}")))?
            .build();
        Ok(SmtpMailer {
            transport,
            from: from
                .parse()
                .map_err(|e| MailError(format!("invalid MAIL_FROM: {e}")))?,
            app_base_url: app_base_url.into(),
        })
    }

    /// An unencrypted connection, for a local test server only.
    pub fn plain_for_tests(
        host: &str,
        port: u16,
        from: &str,
        app_base_url: &str,
    ) -> Result<Self, MailError> {
        let transport = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host)
            .port(port)
            .build();
        Ok(SmtpMailer {
            transport,
            from: from
                .parse()
                .map_err(|e| MailError(format!("invalid sender: {e}")))?,
            app_base_url: app_base_url.into(),
        })
    }
}

#[async_trait]
impl Mailer for SmtpMailer {
    async fn send(&self, message: Message) -> Result<(), MailError> {
        let text = render(&message, &self.app_base_url);
        let email = Email::builder()
            .from(self.from.clone())
            .to(text
                .to
                .parse()
                .map_err(|e| MailError(format!("invalid recipient: {e}")))?)
            .subject(text.subject)
            .body(text.body)
            .map_err(|e| MailError(format!("could not build the email: {e}")))?;
        self.transport
            .send(email)
            .await
            .map(|_| ())
            .map_err(|e| MailError(format!("SMTP send failed: {e}")))
    }
}
