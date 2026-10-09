//! The Ferrix chat example backend (a Slack-lite): channels, messages with attachments and reactions, unread
//! counts, typing indicators, search, `/remind`, and password / email-code / GitHub sign-in.
//!
//! ```sh
//! cargo run -p chat-server -- serve --dev --http 127.0.0.1:8097 \
//!   --codegen-out examples/chat-react/web/src/ferrix
//! ```

mod channels;
mod dev;
mod messages;
mod model;
mod profiles;
mod reactions;
mod typing;

#[cfg(test)]
mod tests;

use ferrix::auth::{AuthConfig, GitHub};

fn main() -> ferrix::Result<()> {
    let app_url = std::env::var("CHAT_APP_URL").unwrap_or_else(|_| "http://localhost:5180".into());
    let mut auth = AuthConfig::password()
        .magic_link()
        // Only when FERRIX_AUTH_GITHUB_CLIENT_ID / _SECRET are set (skipped with a warning otherwise).
        .oauth(GitHub::from_env())
        .redirect_url(app_url);
    if let Some(inbox) = dev::inbox_for_dev_mode() {
        auth = auth.mailer(inbox);
    }
    ferrix::Ferrix::new()
        .auth(auth)
        // Every hour, on the hour (UTC): sweep typing rows whose expiry job didn't delete them.
        .cron::<typing::prune>("prune-typing", "0 0 * * * *", ())
        .run()
}
