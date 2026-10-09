//! A development inbox: in `--dev` without SMTP, auth emails go to ferrix's dev mailer (which prints them to the
//! log) and are also kept in memory, so the e2e test can read sign-in codes with the dev-only `dev:last_code`
//! action. Outside `--dev` (or with SMTP configured) the inbox is never used and the action is refused.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex};

use ferrix::auth::{DevLogMailer, Email, Mailer};
use ferrix::prelude::*;

static INBOX: LazyLock<Arc<DevInbox>> = LazyLock::new(Arc::default);

#[derive(Default)]
pub struct DevInbox {
    emails: Mutex<Vec<Email>>,
}

impl Mailer for DevInbox {
    fn send(&self, email: Email) -> Pin<Box<dyn Future<Output = ferrix::Result<()>> + Send + '_>> {
        let mut emails = self.emails.lock().unwrap_or_else(|e| e.into_inner());
        emails.push(email.clone());
        let excess = emails.len().saturating_sub(200);
        emails.drain(..excess);
        drop(emails);
        // Still printed to the log, like every dev email.
        DevLogMailer.send(email)
    }
}

/// The inbox, for `AuthConfig::dev_mailer` (ferrix only uses it in `--dev` without SMTP).
pub fn inbox() -> Arc<DevInbox> {
    INBOX.clone()
}

/// The newest 6-digit code emailed to `email`.
#[action(dev_only)]
pub async fn last_code(_ctx: &ActionCtx, email: String) -> Result<Option<String>> {
    let email = email.trim().to_lowercase();
    let emails = INBOX.emails.lock().unwrap_or_else(|e| e.into_inner());
    Ok(emails.iter().rev().filter(|e| e.to == email).find_map(|e| code_in(&e.body)))
}

fn code_in(body: &str) -> Option<String> {
    let rest = &body[body.find("Your code: ")? + "Your code: ".len()..];
    let code: String = rest.chars().take_while(char::is_ascii_digit).collect();
    (code.len() == 6).then_some(code)
}
