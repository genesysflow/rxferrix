//! Outgoing email: the [`Mailer`] trait, SMTP via lettre, the dev log mailer and an in-memory mailer for tests.

use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use lettre::message::{Mailbox, MultiPart};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use super::config::SmtpConfig;
use crate::settings::model::EmailTemplate;

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

    /// How long codes stay valid, in words: `15 minutes`, `24 hours` (`{expires}`).
    pub fn validity(self) -> String {
        let minutes = (self.ttl_ms() / 60_000.0) as u64;
        if minutes >= 120 { format!("{} hours", minutes / 60) } else { format!("{minutes} minutes") }
    }

    /// The built-in subject, introduction and call to action.
    fn texts(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Purpose::VerifyEmail => ("Verify your email address", "Please confirm your email address.", "Verify"),
            Purpose::ResetPassword => (
                "Reset your password",
                "Someone (hopefully you) asked to reset your password. If it wasn't you, ignore this email.",
                "Choose a new password",
            ),
            Purpose::SignIn => ("Your sign-in link", "Use this link or code to sign in.", "Sign in"),
        }
    }

    /// The built-in email as a template: the starting point the dashboard offers for a custom one.
    pub fn default_template(self) -> EmailTemplate {
        let (subject, intro, action) = self.texts();
        let html = format!(
            "<p>{intro}</p>\n<p><a href=\"{{link}}\">{action}</a></p>\n<p>Your code: <strong>{{code}}</strong></p>\n\
             <p>This expires in {{expires}}.</p>\n"
        );
        EmailTemplate { subject: subject.to_string(), html }
    }

    /// The built-in plain-text email.
    fn render(self, code: &str, link: Option<&str>) -> (String, String) {
        let (subject, intro, action) = self.texts();
        let mut body = format!("{intro}\n\n");
        if let Some(link) = link {
            body.push_str(&format!("{action}: {link}\n\n"));
        }
        body.push_str(&format!("Your code: {code}\n\nThis expires in {}.\n", self.validity()));
        (subject.to_string(), body)
    }
}

/// The placeholders of auth email templates: `{link}` (empty without a redirect URL), `{code}`, `{expires}`,
/// `{app_name}` and `{email}` (the recipient).
pub const PLACEHOLDERS: &[&str] = &["link", "code", "expires", "app_name", "email"];

/// An auth email to `to`: from `template` when one is set (the HTML, with a plain-text version derived from it), else
/// the built-in plain-text email.
pub(crate) fn auth_email(
    to: String,
    purpose: Purpose,
    code: &str,
    link: Option<&str>,
    template: Option<&EmailTemplate>,
    app_name: &str,
) -> Email {
    let Some(template) = template else {
        let (subject, body) = purpose.render(code, link);
        return Email::new(to, subject, body);
    };
    let expires = purpose.validity();
    let vars = [
        ("link", link.unwrap_or("")),
        ("code", code),
        ("expires", expires.as_str()),
        ("app_name", app_name),
        ("email", to.as_str()),
    ];
    let subject = fill(&template.subject, &vars, false);
    let html = fill(&template.html, &vars, true);
    let body = html_to_text(&html);
    Email::new(to, subject, body).html(html)
}

/// `template` with each known `{name}` replaced by its value (HTML-escaped with `html`), in one pass: a value that
/// contains a placeholder is not expanded again. Other braces (CSS, unknown names) stay as they are.
pub(crate) fn fill(template: &str, vars: &[(&str, &str)], html: bool) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let value = after.find('}').and_then(|j| vars.iter().find(|(k, _)| *k == &after[..j]).map(|(_, v)| (*v, j)));
        match value {
            Some((v, j)) => {
                if html {
                    out.push_str(&escape_html(v));
                } else {
                    out.push_str(v);
                }
                rest = &after[j + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The first `{name}` (lowercase letters and `_`) in `s` that isn't one of [`PLACEHOLDERS`].
pub(crate) fn unknown_placeholder(s: &str) -> Option<&str> {
    let mut rest = s;
    while let Some(i) = rest.find('{') {
        rest = &rest[i + 1..];
        let end = rest.find(|c: char| !(c.is_ascii_lowercase() || c == '_')).unwrap_or(rest.len());
        let name = &rest[..end];
        if !name.is_empty() && rest[end..].starts_with('}') && !PLACEHOLDERS.contains(&name) {
            return Some(name);
        }
    }
    None
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn unescape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let entity = rest[1..].find(';').filter(|&j| j <= 10).map(|j| &rest[1..=j]);
        let decoded = entity.and_then(|e| match e {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => e
                .strip_prefix("#x")
                .or_else(|| e.strip_prefix("#X"))
                .map(|h| u32::from_str_radix(h, 16).ok())
                .unwrap_or_else(|| e.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        });
        match (entity, decoded) {
            (Some(e), Some(c)) => {
                out.push(c);
                rest = &rest[e.len() + 2..];
            }
            _ => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The value of attribute `name` in the inside of a tag (`a href="…" class=x`), unescaped.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(name).map(|i| i + from) {
        from = i + name.len();
        let before_ok = i == 0 || lower.as_bytes()[i - 1].is_ascii_whitespace();
        let rest = tag[from..].trim_start();
        let (true, Some(rest)) = (before_ok, rest.strip_prefix('=')) else { continue };
        let rest = rest.trim_start();
        let value = match rest.chars().next() {
            Some(q @ ('"' | '\'')) => rest[1..].split(q).next().unwrap_or(""),
            _ => rest.split(|c: char| c.is_whitespace() || c == '>').next().unwrap_or(""),
        };
        return Some(unescape_html(value));
    }
    None
}

/// A plain-text version of an HTML email, for clients that don't show HTML: tags dropped (`<style>`, `<script>`,
/// `<head>` with their content), line breaks for block elements, `text (url)` for links, entities decoded.
pub(crate) fn html_to_text(html: &str) -> String {
    const BLOCKS: &[&str] = &[
        "p",
        "div",
        "br",
        "tr",
        "li",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "table",
        "ul",
        "ol",
        "blockquote",
        "hr",
        "section",
        "header",
        "footer",
        "article",
        "pre",
    ];
    let mut out = String::new();
    let mut text = String::new();
    // Text runs collapse their whitespace, as browsers show them.
    let flush = |text: &mut String, out: &mut String| {
        let decoded = unescape_html(text);
        for (i, word) in decoded.split_whitespace().enumerate() {
            let spaced = i > 0 || decoded.starts_with(char::is_whitespace);
            if spaced && !out.is_empty() && !out.ends_with(['\n', ' ']) {
                out.push(' ');
            }
            out.push_str(word);
        }
        if decoded.ends_with(char::is_whitespace) && !out.is_empty() && !out.ends_with(['\n', ' ']) {
            out.push(' ');
        }
        text.clear();
    };
    let mut links: Vec<(usize, Option<String>)> = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find('<') {
        text.push_str(&rest[..i]);
        rest = &rest[i..];
        if let Some(after) = rest.strip_prefix("<!--") {
            rest = after.find("-->").map_or("", |j| &after[j + 3..]);
            continue;
        }
        let Some(end) = rest.find('>') else {
            text.push_str(rest);
            rest = "";
            break;
        };
        let inner = &rest[1..end];
        rest = &rest[end + 1..];
        let closing = inner.starts_with('/');
        let name: String = inner
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        if !closing && matches!(name.as_str(), "style" | "script" | "head" | "title") {
            let close = format!("</{name}");
            rest = rest.to_ascii_lowercase().find(&close).map_or("", |j| {
                let tail = &rest[j..];
                tail.find('>').map_or("", |k| &tail[k + 1..])
            });
            continue;
        }
        flush(&mut text, &mut out);
        if name == "a" {
            if closing {
                if let Some((start, Some(href))) = links.pop() {
                    let label = out.get(start..).unwrap_or("").trim();
                    if label != href && !href.is_empty() && !href.starts_with('#') {
                        if label.is_empty() {
                            out.push_str(&href);
                        } else {
                            let trimmed = out.trim_end().len();
                            out.truncate(trimmed);
                            out.push_str(&format!(" ({href})"));
                        }
                    }
                }
            } else {
                links.push((out.len(), attribute(&inner[1..], "href")));
            }
        } else if BLOCKS.contains(&name.as_str()) {
            let trimmed = out.trim_end_matches(' ').len();
            out.truncate(trimmed);
            out.push('\n');
            if name == "li" && !closing {
                out.push_str("- ");
            }
        }
    }
    text.push_str(rest);
    flush(&mut text, &mut out);
    let lines: Vec<&str> = out.lines().map(str::trim).collect();
    let mut result = String::new();
    let mut blank = 0;
    for line in lines {
        if line.is_empty() {
            blank += 1;
            continue;
        }
        if !result.is_empty() {
            result.push_str(if blank > 0 { "\n\n" } else { "\n" });
        }
        blank = 0;
        result.push_str(line);
    }
    result.push('\n');
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_fill_placeholders_once_and_escape_them_in_html() {
        let vars = [("code", "123456"), ("app_name", "Tom & Jerry {code}"), ("link", "https://a.test/?x=1&y=2#t=k")];
        assert_eq!(fill("{app_name}: {code}", &vars, false), "Tom & Jerry {code}: 123456");
        assert_eq!(
            fill(r#"<a href="{link}">{app_name}</a> p {{ color: red }} {nope}"#, &vars, true),
            r#"<a href="https://a.test/?x=1&amp;y=2#t=k">Tom &amp; Jerry {code}</a> p {{ color: red }} {nope}"#
        );
        assert_eq!(unknown_placeholder("Hi {app_name}, {code} {expires} {link} {email}"), None);
        assert_eq!(unknown_placeholder("p { color: red } {appname}"), Some("appname"));
        assert_eq!(unknown_placeholder("{ code } {Code} {}"), None);
    }

    #[test]
    fn html_becomes_readable_text() {
        let html = r#"<html><head><title>x</title><style>p { color: red }</style></head><body>
            <!-- hidden --><h1>Welcome   to&nbsp;Acme</h1>
            <p>Click <a href="https://a.test/?x=1&amp;y=2">here</a> or
            <a href="https://b.test">https://b.test</a>.<br>Code: <strong>123456</strong></p>
            <ul><li>one</li></ul><script>alert(1)</script></body></html>"#;
        assert_eq!(
            html_to_text(html),
            "Welcome to Acme\n\nClick here (https://a.test/?x=1&y=2) or https://b.test.\nCode: 123456\n\n- one\n"
        );
        assert_eq!(unescape_html("&lt;&#65;&#x42;&bogus; & &amp;"), "<AB&bogus; & &");
    }

    #[test]
    fn auth_emails_use_the_template_or_the_built_in_text() {
        let link = Some("https://app.test/#rxferrix_magic_token=t");
        let built_in = auth_email("a@b.co".into(), Purpose::SignIn, "123456", link, None, "Acme");
        assert_eq!((built_in.subject.as_str(), built_in.html.is_none()), ("Your sign-in link", true));
        assert!(built_in.body.contains("Your code: 123456") && built_in.body.contains("expires in 15 minutes"));

        let t = EmailTemplate {
            subject: "{app_name}: sign in as {email}".into(),
            html: r#"<p>Code <b>{code}</b>, valid {expires}.</p><p><a href="{link}">Sign in</a></p>"#.into(),
        };
        let e = auth_email("a@b.co".into(), Purpose::SignIn, "123456", link, Some(&t), "Acme");
        assert_eq!(e.subject, "Acme: sign in as a@b.co");
        assert_eq!(
            e.html.as_deref(),
            Some(
                r#"<p>Code <b>123456</b>, valid 15 minutes.</p><p><a href="https://app.test/#rxferrix_magic_token=t">Sign in</a></p>"#
            )
        );
        assert_eq!(e.body, "Code 123456, valid 15 minutes.\n\nSign in (https://app.test/#rxferrix_magic_token=t)\n");

        // The default template carries what the built-in email does.
        let d = Purpose::VerifyEmail.default_template();
        let e = auth_email("a@b.co".into(), Purpose::VerifyEmail, "654321", link, Some(&d), "");
        assert_eq!(e.subject, "Verify your email address");
        assert!(e.body.contains("Your code: 654321") && e.body.contains("expires in 24 hours"), "{}", e.body);
        assert!(e.body.contains("Verify (https://app.test/#rxferrix_magic_token=t)"), "{}", e.body);
    }
}
