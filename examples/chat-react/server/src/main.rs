//! The RxFerrix chat example backend (a Slack-lite): channels, messages with attachments and reactions, unread
//! counts, typing indicators, search, `/remind`, and password / email-code / GitHub sign-in. Live message and member
//! counts come from aggregates; the Insights view runs SQL over the analytics copy (build with `--features olap`
//! and serve with `--olap`); the announcement banner is an environment variable (`ctx.env`).
//!
//! ```sh
//! cargo run -p chat-server -- serve --dev --http 127.0.0.1:8097 \
//!   --codegen-out examples/chat-react/web/src/rxferrix
//! ```

mod announcement;
mod channels;
mod dev;
mod insights;
mod messages;
mod model;
mod profiles;
mod reactions;
mod typing;

#[cfg(test)]
mod tests;

use rxferrix::auth::{AuthConfig, GitHub};

fn main() -> rxferrix::Result<()> {
    let app_url = std::env::var("CHAT_APP_URL").unwrap_or_else(|_| "http://localhost:5180".into());
    let auth = AuthConfig::password()
        .magic_link()
        // Only when RXFERRIX_AUTH_GITHUB_CLIENT_ID / _SECRET are set (skipped with a warning otherwise).
        .oauth(GitHub::from_env())
        .redirect_url(app_url)
        // In --dev without SMTP: emails are logged and kept for `dev:last_code` (the e2e test reads codes there).
        .dev_mailer(dev::inbox());
    let mut app = rxferrix::RxFerrix::new()
        .auth(auth)
        // Every hour, on the hour (UTC): sweep typing rows whose expiry job didn't delete them.
        .cron::<typing::prune>("prune-typing", "0 0 * * * *", ());
    // CHAT_WEB_DIR=examples/chat-react/web/dist: serve the built app too, so app, API and dashboard share one origin
    // (instead of Vite proxying /api in development).
    if let Some(dir) = std::env::var_os("CHAT_WEB_DIR").filter(|d| !d.is_empty()) {
        app = app.static_dir(dir);
    }
    app.run()
}
