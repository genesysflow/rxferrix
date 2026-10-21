//! Request rate limits set at runtime (the `rateLimits` setting; dashboard: Settings → Rate limiting), per client IP
//! as the trusted proxy headers resolve it (`crate::auth::http::client_ip`; IPv6 clients by their /64).
//!
//! A rule's label says what it limits:
//! - `/…`: HTTP requests whose path starts with it (`/api/`, `/api/storage/upload`, an app route, static files),
//!   checked by a middleware before the handler runs ([`limit_requests`]). The longest matching prefix decides, and
//!   the requests it counts don't also count against shorter ones.
//! - `module:function`, either side `*`, or a lone `*`: app functions called by clients, over HTTP (`/api/query`,
//!   `/api/mutation`, `/api/action`) and over the WebSocket (mutations and actions; query subscriptions are not
//!   counted). The most specific match decides: `messages:send`, then `messages:*`, then `*:send`, then `*`.
//!
//! A rule for `guest`s or `signedIn` users goes before an `all` rule with the same label. Each rule is a token bucket
//! per client: up to `maxRequests` at once, refilled evenly over `intervalSecs`. Over the limit: 429 `rate_limited`
//! with `Retry-After` over HTTP, a `rate_limited` error for that mutation or action over the WebSocket.
//!
//! Never limited, so a rule can't lock administrators or monitoring out: the dashboard (`/_/`, `/api/admin/`,
//! `/api/auth/admin/`), `/api/health`, `/metrics`, system functions (`_system/…`) and calls by administrators.
//! `/api/sync` (WebSocket connections, the dashboard's included) and `/api/auth/refresh` only count against a rule
//! naming them (`/api/sync`), not against `/api/` or `/` ([`DEDICATED_PATHS`]).
//!
//! **Cost.** Turned off, or without rules of a kind, a request pays one atomic load. Otherwise: a scan of the rules,
//! the client IP from the headers, and one token bucket update under one of 32 shard locks
//! (`crate::auth::rate_limit`). Telling guests from signed-in users on a path rule checks the token's signature, but
//! reads nothing from the database. Saving rules keeps the counts of the rules that didn't change.

use std::hash::{Hash, Hasher};
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use super::model::{RateLimitAudience, RateLimitRule, RateLimitSettings};
use crate::error::{ClientError, ErrorKind};
use crate::prelude::*;
use crate::runtime::{App, Identity};

pub(crate) const MAX_RULES: usize = 50;
pub(crate) const MAX_REQUESTS: u32 = 1_000_000;
pub(crate) const MAX_INTERVAL_SECS: u32 = 24 * 3600;
const MAX_LABEL: usize = 256;

/// Paths no rule applies to: the dashboard, its API and sign-in, health checks and metrics scrapes.
fn exempt_path(path: &str) -> bool {
    path == "/_"
        || path.starts_with("/_/")
        || path.starts_with("/api/admin/")
        || path.starts_with("/api/auth/admin/")
        || path == "/api/health"
        || path == "/metrics"
}

/// Paths the dashboard shares with apps, counted only by a rule written for them (its label starts with one of these,
/// e.g. `/api/sync`), never by a broader prefix such as `/api/` or `/`: otherwise an ordinary API limit would also
/// throttle the dashboard's WebSocket connections and session refreshes, and lock administrators out. Both have their
/// own limits already (one connection per page; refreshes in `AuthConfig::rate_limits`).
const DEDICATED_PATHS: &[&str] = &["/api/sync", "/api/auth/refresh"];

// ---------------------------------------------------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Target {
    /// A path prefix.
    Path(String),
    /// `module:name`; `None` for `*`.
    Function { module: Option<String>, name: Option<String> },
}

fn parse_label(label: &str) -> std::result::Result<Target, String> {
    let bad = || {
        format!(
            "rate limit `{label}`: use a path starting with `/` (`/api/`) or a function such as `messages:send`, \
             `messages:*`, `*:send` or `*`"
        )
    };
    if label.len() > MAX_LABEL {
        return Err(format!("rate limit labels are at most {MAX_LABEL} characters"));
    }
    if label.starts_with('/') {
        if label.contains('*') {
            let prefix = label.trim_end_matches('*');
            return Err(format!("rate limit `{label}`: path rules match by prefix, write `{prefix}`"));
        }
        if !label.bytes().all(|b| b.is_ascii_graphic() && b != b'?' && b != b'#') {
            return Err(format!("rate limit `{label}`: a path has no spaces, query or fragment"));
        }
        return Ok(Target::Path(label.to_string()));
    }
    if label == "*" {
        return Ok(Target::Function { module: None, name: None });
    }
    let (module, name) = label.split_once(':').ok_or_else(bad)?;
    let module_ok = module == "*"
        || (!module.is_empty()
            && !module.starts_with("_system")
            && module.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'/' | b'.' | b'-')));
    let name_ok = name == "*" || (!name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
    if !module_ok || !name_ok {
        return Err(bad());
    }
    let side = |s: &str| (s != "*").then(|| s.to_string());
    Ok(Target::Function { module: side(module), name: side(name) })
}

/// The rules as entered, trimmed and checked; `None` when there is nothing to keep (off and no rules).
pub(crate) fn from_input(input: RateLimitSettings) -> Result<Option<RateLimitSettings>> {
    let invalid = |m: String| -> Error { UserError::new("invalid_settings", m).into() };
    if input.rules.len() > MAX_RULES {
        return Err(invalid(format!("at most {MAX_RULES} rate limit rules")));
    }
    let mut rules: Vec<RateLimitRule> = Vec::with_capacity(input.rules.len());
    for rule in input.rules {
        let label = rule.label.trim().to_string();
        if label.is_empty() {
            return Err(invalid("a rate limit rule needs a label (a path or a function)".into()));
        }
        parse_label(&label).map_err(invalid)?;
        if !(1..=MAX_REQUESTS).contains(&rule.max_requests) {
            return Err(invalid(format!("rate limit `{label}`: max requests must be between 1 and {MAX_REQUESTS}")));
        }
        if !(1..=MAX_INTERVAL_SECS).contains(&rule.interval_secs) {
            return Err(invalid(format!(
                "rate limit `{label}`: the interval must be between 1 and {MAX_INTERVAL_SECS} seconds"
            )));
        }
        if rules.iter().any(|r| r.label == label && r.audience == rule.audience) {
            return Err(invalid(format!("rate limit `{label}` is listed twice for the same users")));
        }
        rules.push(RateLimitRule { label, ..rule });
    }
    Ok((input.enabled || !rules.is_empty()).then_some(RateLimitSettings { enabled: input.enabled, rules }))
}

#[derive(Debug)]
pub(crate) struct Compiled {
    /// Identifies the rule's buckets: a hash of everything about it, so changing a rule starts it afresh while the
    /// unchanged ones keep their counts across saves.
    id: u64,
    target: Target,
    audience: RateLimitAudience,
    burst: u32,
    per_sec: f64,
}

impl Compiled {
    fn applies_to(&self, signed_in: bool) -> bool {
        match self.audience {
            RateLimitAudience::All => true,
            RateLimitAudience::Guest => !signed_in,
            RateLimitAudience::SignedIn => signed_in,
        }
    }

    fn matches_path(&self, path: &str) -> bool {
        let Target::Path(prefix) = &self.target else { return false };
        path.starts_with(prefix.as_str())
            && DEDICATED_PATHS.iter().all(|d| !path.starts_with(d) || prefix.starts_with(d))
    }

    fn matches_function(&self, path: &str) -> bool {
        let Target::Function { module, name } = &self.target else { return false };
        let (m, n) = path.split_once(':').unwrap_or((path, ""));
        module.as_deref().is_none_or(|x| x == m) && name.as_deref().is_none_or(|x| x == n)
    }
}

/// The rules in effect (none when the setting is off), most specific first.
#[derive(Debug, Default)]
pub(crate) struct RateLimitRules {
    paths: Vec<Compiled>,
    functions: Vec<Compiled>,
}

impl RateLimitRules {
    pub(crate) fn build(stored: Option<&RateLimitSettings>) -> RateLimitRules {
        let mut out = RateLimitRules::default();
        let Some(settings) = stored.filter(|s| s.enabled) else { return out };
        for rule in &settings.rules {
            // Stored rules were validated on the way in; skip any that a hand-edited row broke.
            let Ok(target) = parse_label(&rule.label) else { continue };
            let mut h = std::hash::DefaultHasher::new();
            (&rule.label, rule.max_requests, rule.interval_secs, rule.audience).hash(&mut h);
            let compiled = Compiled {
                id: h.finish(),
                target,
                audience: rule.audience,
                burst: rule.max_requests.max(1),
                per_sec: f64::from(rule.max_requests.max(1)) / f64::from(rule.interval_secs.max(1)),
            };
            match compiled.target {
                Target::Path(_) => out.paths.push(compiled),
                Target::Function { .. } => out.functions.push(compiled),
            }
        }
        let general = |r: &Compiled| r.audience == RateLimitAudience::All;
        out.paths.sort_by_key(|r| {
            let Target::Path(p) = &r.target else { unreachable!() };
            (std::cmp::Reverse(p.len()), general(r))
        });
        out.functions.sort_by_key(|r| {
            let Target::Function { module, name } = &r.target else { unreachable!() };
            (std::cmp::Reverse(2 * u8::from(module.is_some()) + u8::from(name.is_some())), general(r))
        });
        out
    }

    pub(crate) fn has_paths(&self) -> bool {
        !self.paths.is_empty()
    }

    pub(crate) fn has_functions(&self) -> bool {
        !self.functions.is_empty()
    }

    /// The rule for a call of function `path`.
    fn function_rule(&self, path: &str, signed_in: bool) -> Option<&Compiled> {
        self.functions.iter().find(|r| r.matches_function(path) && r.applies_to(signed_in))
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------------------------------------------------

/// A request over its limit, and when to try again.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RateLimited {
    pub retry_after: Duration,
}

impl RateLimited {
    fn retry_secs(&self) -> u64 {
        self.retry_after.as_secs_f64().ceil().max(1.0) as u64
    }

    /// The error a WebSocket client gets for its mutation or action.
    pub(crate) fn client_error(&self) -> ClientError {
        ClientError {
            kind: ErrorKind::User,
            message: "Too many requests; try again later".into(),
            data: Some(json!({ "code": "rate_limited", "data": { "retryAfterSecs": self.retry_secs() } })),
        }
    }
}

/// 429 with `Retry-After`, in the function API's error shape.
impl IntoResponse for RateLimited {
    fn into_response(self) -> Response {
        let error = self.client_error();
        let body = json!({ "status": "error", "errorMessage": error.message, "errorData": error.data, "logLines": [] });
        let mut response = (StatusCode::TOO_MANY_REQUESTS, axum::Json(body)).into_response();
        response.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(self.retry_secs()));
        response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
    }
}

/// The bucket key of a client: its address, IPv6 ones by their /64 (what one subscriber usually gets).
fn client_key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from_bits(v6.to_bits() & !((1u128 << 64) - 1))),
        v4 => v4,
    }
}

fn take(app: &App, rule: &Compiled, ip: Option<IpAddr>) -> std::result::Result<(), RateLimited> {
    let ip = client_key(ip.unwrap_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)));
    app.auth()
        .request_limiter
        .take((rule.id, ip), rule.burst, rule.per_sec)
        .map_err(|retry_after| RateLimited { retry_after })
}

/// Count a client's call of the function `path` against the function rules; `ip` is only resolved when a rule
/// applies.
pub(crate) fn check_function(
    app: &App,
    path: &str,
    identity: &Identity,
    ip: impl FnOnce() -> Option<IpAddr>,
) -> std::result::Result<(), RateLimited> {
    if !app.auth().function_limits_on() || identity.admin_id.is_some() || path.starts_with("_system/") {
        return Ok(());
    }
    let rules = app.auth().current();
    match rules.rate_limits.function_rule(path, identity.user_id.is_some()) {
        Some(rule) => take(app, rule, ip()),
        None => Ok(()),
    }
}

/// The client IP of an HTTP request, as the trusted proxy headers in effect resolve it.
pub(crate) fn request_ip(app: &App, parts: &axum::http::Extensions, headers: &axum::http::HeaderMap) -> Option<IpAddr> {
    let peer = parts.get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip());
    crate::auth::http::client_ip(&app.auth().current().proxy, headers, peer).0
}

/// Middleware: count the request against the path rules (see the module docs).
pub(crate) async fn limit_requests(State(app): State<App>, request: Request, next: Next) -> Response {
    if !app.auth().path_limits_on() {
        return next.run(request).await;
    }
    let path = request.uri().path();
    if exempt_path(path) {
        return next.run(request).await;
    }
    let effective = app.auth().current();
    let rules = &effective.rate_limits;
    let Some(first) = rules.paths.iter().find(|r| r.matches_path(path)) else {
        return next.run(request).await;
    };
    // Borrow the parts, not the request: its body isn't `Sync`, and this future must be `Send`.
    let (headers, peer) = (request.headers(), request.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip()));
    let caller = async || match crate::auth::http::bearer_token(headers) {
        Some(token) => app.auth().token_caller(&app, token).await,
        None => TokenCaller::Guest,
    };
    let mut known = None;
    let rule = if first.audience == RateLimitAudience::All {
        Some(first)
    } else {
        // Only now is it worth checking the token: whether the caller is signed in picks the rule.
        let c = *known.insert(caller().await);
        if c == TokenCaller::Admin {
            return next.run(request).await;
        }
        rules.paths.iter().find(|r| r.matches_path(path) && r.applies_to(c == TokenCaller::User))
    };
    if let Some(rule) = rule {
        let ip = crate::auth::http::client_ip(&effective.proxy, headers, peer).0;
        if let Err(limited) = take(&app, rule, ip) {
            // Administrators are never locked out; checked only now, so allowed requests don't pay for it.
            let admin = match known {
                Some(c) => c == TokenCaller::Admin,
                None => caller().await == TokenCaller::Admin,
            };
            if !admin {
                return limited.into_response();
            }
        }
    }
    drop(effective);
    next.run(request).await
}

/// Who an access token says the caller is, from its signature and expiry alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenCaller {
    Guest,
    User,
    Admin,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(label: &str, audience: RateLimitAudience) -> RateLimitRule {
        RateLimitRule { label: label.into(), max_requests: 10, interval_secs: 1, audience }
    }

    fn rules(list: &[(&str, RateLimitAudience)]) -> RateLimitRules {
        let rules = list.iter().map(|(l, a)| rule(l, *a)).collect();
        RateLimitRules::build(Some(&RateLimitSettings { enabled: true, rules }))
    }

    fn label_of(r: Option<&Compiled>) -> Option<String> {
        r.map(|r| match &r.target {
            Target::Path(p) => p.clone(),
            Target::Function { module, name } => {
                format!("{}:{}", module.as_deref().unwrap_or("*"), name.as_deref().unwrap_or("*"))
            }
        })
    }

    #[test]
    fn labels_are_paths_or_function_patterns() {
        assert_eq!(parse_label("/api/"), Ok(Target::Path("/api/".into())));
        assert_eq!(parse_label("*"), Ok(Target::Function { module: None, name: None }));
        assert_eq!(parse_label("*:create"), Ok(Target::Function { module: None, name: Some("create".into()) }));
        assert_eq!(
            parse_label("chat/messages:*"),
            Ok(Target::Function { module: Some("chat/messages".into()), name: None })
        );
        assert!(parse_label("/api/*").unwrap_err().contains("write `/api/`"));
        for bad in ["messages", "messages:", ":send", "a b:c", "_system/settings:update", "/a b", "/a?b=1", "m:se-nd"] {
            assert!(parse_label(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn input_is_trimmed_and_checked() {
        use RateLimitAudience::*;
        let input = |rules: Vec<RateLimitRule>| RateLimitSettings { enabled: true, rules };
        let ok = from_input(input(vec![rule(" /api/ ", All), rule("/api/", Guest)])).unwrap().unwrap();
        assert_eq!(ok.rules[0].label, "/api/");
        assert!(from_input(input(vec![rule("/api/", All), rule("/api/ ", All)])).is_err());
        assert!(from_input(input(vec![RateLimitRule { max_requests: 0, ..rule("*", All) }])).is_err());
        assert!(from_input(input(vec![RateLimitRule { interval_secs: 86_401, ..rule("*", All) }])).is_err());
        assert!(from_input(input(vec![rule(" ", All)])).is_err());
        assert!(from_input(input((0..51).map(|i| rule(&format!("/p{i}"), All)).collect())).is_err());
        assert_eq!(from_input(RateLimitSettings { enabled: false, rules: vec![] }).unwrap(), None);
    }

    #[test]
    fn the_most_specific_rule_decides() {
        use RateLimitAudience::*;
        let r = rules(&[("/api/", All), ("/api/storage/", All), ("/api/", Guest), ("*", All), ("*:send", All)]);
        let path = |p: &str| label_of(r.paths.iter().find(|x| x.matches_path(p)));
        assert_eq!(path("/api/storage/upload").as_deref(), Some("/api/storage/"));
        assert_eq!(path("/api/query").as_deref(), Some("/api/"));
        assert_eq!(path("/index.html"), None);
        // The guest rule goes first; signed-in callers fall through to the general one.
        assert_eq!(r.paths.iter().find(|x| x.matches_path("/api/query")).unwrap().audience, Guest);
        assert_eq!(r.paths.iter().find(|x| x.matches_path("/api/query") && x.applies_to(true)).unwrap().audience, All);

        let r = rules(&[("*", All), ("*:send", All), ("messages:*", All), ("messages:send", SignedIn)]);
        let f = |p: &str, signed_in| label_of(r.function_rule(p, signed_in));
        assert_eq!(f("messages:send", true).as_deref(), Some("messages:send"));
        assert_eq!(f("messages:send", false).as_deref(), Some("messages:*"));
        assert_eq!(f("chat:send", false).as_deref(), Some("*:send"));
        assert_eq!(f("chat:list", false).as_deref(), Some("*:*"));
        // Module paths match whole.
        assert_eq!(f("messages/extra:list", false).as_deref(), Some("*:*"));
    }

    #[test]
    fn websocket_connections_and_refreshes_only_count_against_their_own_rules() {
        use RateLimitAudience::*;
        let r = rules(&[("/", All), ("/api/", All), ("/api/auth/", All)]);
        let path = |p: &str| label_of(r.paths.iter().find(|x| x.matches_path(p)));
        assert_eq!(path("/api/sync"), None);
        assert_eq!(path("/api/auth/refresh"), None);
        assert_eq!(path("/api/auth/signin").as_deref(), Some("/api/auth/"));
        let r = rules(&[("/api/", All), ("/api/sync", All)]);
        let path = |p: &str| label_of(r.paths.iter().find(|x| x.matches_path(p)));
        assert_eq!(path("/api/sync").as_deref(), Some("/api/sync"));
    }

    #[test]
    fn disabled_settings_have_no_rules() {
        let settings = RateLimitSettings { enabled: false, rules: vec![rule("/api/", RateLimitAudience::All)] };
        let r = RateLimitRules::build(Some(&settings));
        assert!(!r.has_paths() && !r.has_functions());
    }

    #[test]
    fn ipv6_clients_share_their_64() {
        let a: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:bbbb::9".parse().unwrap();
        let c: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(client_key(a), client_key(b));
        assert_ne!(client_key(a), client_key(c));
        let mapped: IpAddr = "::ffff:203.0.113.7".parse().unwrap();
        assert_eq!(client_key(mapped), "203.0.113.7".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn dashboard_and_probes_are_exempt() {
        for p in ["/_", "/_/settings", "/api/admin/stats", "/api/auth/admin/signin", "/api/health", "/metrics"] {
            assert!(exempt_path(p), "{p}");
        }
        for p in ["/api/query", "/_app", "/api/healthz", "/"] {
            assert!(!exempt_path(p), "{p}");
        }
    }
}
