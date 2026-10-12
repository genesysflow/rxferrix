//! Outgoing email: the [`Mailer`] trait, SMTP via lettre, the dev log mailer and an in-memory mailer for tests.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use lettre::message::{Mailbox, MultiPart};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use super::config::SmtpConfig;

/// One email: a plain-text body, and optionally an HTML version of it (sent as `multipart/alternative`). `to` is an
/// address, or a name and an address (`Ana Pop <ana@example.com>`).
#[derive(Clone, PartialEq, Eq)]
pub struct Email {
    pub to: String,
    pub subject: String,
    pub body: String,
    pub html: Option<String>,
}

impl Email {
    /// A plain-text email: `Email::new(to, "Your booking", text)`.
    pub fn new(to: impl Into<String>, subject: impl Into<String>, body: impl Into<String>) -> Self {
        Email { to: to.into(), subject: subject.into(), body: body.into(), html: None }
    }

    /// Add an HTML version; `body` stays the plain-text one, for clients that don't show HTML.
    pub fn html(mut self, html: impl Into<String>) -> Self {
        self.html = Some(html.into());
        self
    }
}

impl std::fmt::Debug for Email {
    // The body carries one-time codes and links: keep it out of debug output.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Email").field("to", &self.to).field("subject", &self.subject).finish_non_exhaustive()
    }
}

/// Sends email. Implement it to plug in another transport (an HTTP email API, a test capture, ...).
pub trait Mailer: Send + Sync + 'static {
    fn send(&self, email: Email) -> BoxFuture<'_, crate::Result<()>>;
}

/// SMTP through lettre (async, pooled, rustls).
pub struct SmtpMailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
}

impl SmtpMailer {
    pub fn new(config: &SmtpConfig) -> crate::Result<Self> {
        let transport = AsyncSmtpTransport::<Tokio1Executor>::from_url(&config.url)
            .map_err(|e| anyhow::anyhow!("invalid SMTP URL: {e}"))?
            .build();
        let from = config.from.parse().map_err(|e| anyhow::anyhow!("invalid SMTP from address: {e}"))?;
        Ok(SmtpMailer { transport, from })
    }
}

impl Mailer for SmtpMailer {
    fn send(&self, email: Email) -> BoxFuture<'_, crate::Result<()>> {
        Box::pin(async move {
            let to: Mailbox = email.to.parse().map_err(|e| anyhow::anyhow!("invalid recipient: {e}"))?;
            let builder = Message::builder().from(self.from.clone()).to(to).subject(email.subject);
            let message = match email.html {
                Some(html) => builder.multipart(MultiPart::alternative_plain_html(email.body, html))?,
                None => builder.body(email.body)?,
            };
            self.transport.send(message).await?;
            Ok(())
        })
    }
}

/// The mailer of `--dev` without SMTP: prints every email, links and codes included, to the log. Replace it with
/// [`super::AuthConfig::dev_mailer`]; a custom mailer can forward to it (`DevLogMailer.send(email)`) to keep the
/// log lines.
#[derive(Debug, Clone, Copy, Default)]
pub struct DevLogMailer;

impl Mailer for DevLogMailer {
    fn send(&self, email: Email) -> BoxFuture<'_, crate::Result<()>> {
        Box::pin(async move {
            tracing::info!(
                "dev mailer (no SMTP configured), email to {}:\n  Subject: {}\n\n{}\n",
                email.to,
                email.subject,
                email.body.lines().map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n")
            );
            if let Some(html) = &email.html {
                tracing::info!("dev mailer: the email to {} also has an HTML version ({} bytes)", email.to, html.len());
            }
            Ok(())
        })
    }
}

/// Keeps every email in memory. For tests: `AuthConfig::password().mailer(mailer.clone())`, then read
/// [`MemoryMailer::emails`].
#[derive(Default, Clone)]
pub struct MemoryMailer {
    sent: Arc<Mutex<Vec<Email>>>,
}

impl MemoryMailer {
    pub fn new() -> Arc<Self> {
        Arc::new(MemoryMailer::default())
    }

    /// Every email sent so far.
    pub fn emails(&self) -> Vec<Email> {
        self.sent.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The most recent email to `to`.
    pub fn last_to(&self, to: &str) -> Option<Email> {
        self.emails().into_iter().rev().find(|e| e.to == to)
    }
}

impl Mailer for MemoryMailer {
    fn send(&self, email: Email) -> BoxFuture<'_, crate::Result<()>> {
        self.sent.lock().unwrap_or_else(|e| e.into_inner()).push(email);
        Box::pin(async { Ok(()) })
    }
}

/// What an email with a one-time code is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    VerifyEmail,
    ResetPassword,
    SignIn,
}

impl Purpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Purpose::VerifyEmail => "verify_email",
            Purpose::ResetPassword => "reset_password",
            Purpose::SignIn => "sign_in",
        }
    }

    /// Fragment key of the link token (`<redirect>#<key>=<token>`), read by the TS client's
    /// `handleRedirectCallback()`.
    pub fn fragment_key(self) -> &'static str {
        match self {
            Purpose::VerifyEmail => "rxferrix_verify_token",
            Purpose::ResetPassword => "rxferrix_reset_token",
            Purpose::SignIn => "rxferrix_magic_token",
        }
    }

    /// How long codes stay valid.
    pub fn ttl_ms(self) -> f64 {
        match self {
            Purpose::VerifyEmail => 24.0 * 3600.0 * 1000.0,
            Purpose::ResetPassword => 30.0 * 60.0 * 1000.0,
            Purpose::SignIn => 15.0 * 60.0 * 1000.0,
        }
    }

    pub(crate) fn render(self, code: &str, link: Option<&str>) -> (String, String) {
        let minutes = (self.ttl_ms() / 60_000.0) as u64;
        let validity = if minutes >= 120 { format!("{} hours", minutes / 60) } else { format!("{minutes} minutes") };
        let (subject, intro, action) = match self {
            Purpose::VerifyEmail => ("Verify your email address", "Please confirm your email address.", "Verify"),
            Purpose::ResetPassword => (
                "Reset your password",
                "Someone (hopefully you) asked to reset your password. If it wasn't you, ignore this email.",
                "Choose a new password",
            ),
            Purpose::SignIn => ("Your sign-in link", "Use this link or code to sign in.", "Sign in"),
        };
        let mut body = format!("{intro}\n\n");
        if let Some(link) = link {
            body.push_str(&format!("{action}: {link}\n\n"));
        }
        body.push_str(&format!("Your code: {code}\n\nThis expires in {validity}.\n"));
        (subject.to_string(), body)
    }
}
