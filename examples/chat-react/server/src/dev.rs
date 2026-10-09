//! A development inbox: in `--dev` without SMTP, auth emails are printed to the log (like ferrix's own dev mailer)
//! and also kept in memory, so the e2e test can read sign-in codes with the `dev:last_code` action. Outside that
//! mode the action refuses to run.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};

use ferrix::auth::{Email, Mailer};
use ferrix::log;
use ferrix::prelude::*;

use crate::model::user_error;

static INBOX: OnceLock<Arc<DevInbox>> = OnceLock::new();

#[derive(Default)]
pub struct DevInbox {
    emails: Mutex<Vec<Email>>,
}

impl Mailer for DevInbox {
    fn send(&self, email: Email) -> Pin<Box<dyn Future<Output = ferrix::Result<()>> + Send + '_>> {
        log::info!("dev inbox, email to {}:\n  Subject: {}\n\n{}\n", email.to, email.subject, email.body);
        let mut emails = self.emails.lock().unwrap_or_else(|e| e.into_inner());
        emails.push(email);
        let excess = emails.len().saturating_sub(200);
        emails.drain(..excess);
        Box::pin(async { Ok(()) })
    }
}

/// The dev inbox, when this process runs `serve --dev` without SMTP configured. Read from the command line because
/// ferrix doesn't tell the app whether it runs in dev mode.
pub fn inbox_for_dev_mode() -> Option<Arc<DevInbox>> {
    let args: Vec<String> = std::env::args().collect();
    let dev = args.iter().any(|a| a == "--dev");
    let smtp = args.iter().any(|a| a.starts_with("--smtp-url")) || std::env::var_os("FERRIX_SMTP_URL").is_some();
    if !dev || smtp {
        return None;
    }
    Some(INBOX.get_or_init(Arc::default).clone())
}

/// The newest 6-digit code emailed to `email` (dev mode only).
#[action]
pub async fn last_code(_ctx: &ActionCtx, email: String) -> Result<Option<String>> {
    let Some(inbox) = INBOX.get() else {
        return Err(user_error("dev_only", "The dev inbox only exists in --dev without SMTP"));
    };
    let email = email.trim().to_lowercase();
    let emails = inbox.emails.lock().unwrap_or_else(|e| e.into_inner());
    Ok(emails.iter().rev().filter(|e| e.to == email).find_map(|e| code_in(&e.body)))
}

fn code_in(body: &str) -> Option<String> {
    let rest = &body[body.find("Your code: ")? + "Your code: ".len()..];
    let code: String = rest.chars().take_while(char::is_ascii_digit).collect();
    (code.len() == 6).then_some(code)
}
