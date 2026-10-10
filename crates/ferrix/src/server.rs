//! HTTP + WebSocket server (axum).
//!
//! - `GET  /api/sync`: the WebSocket sync protocol (docs/PROTOCOL.md).
//! - `POST /api/query | /api/mutation | /api/action`: the HTTP fallback, `{ "path", "args" }`.
//! - `GET  /api/health`.
//! - `/api/auth/*`: authentication (`crate::auth::http`).
//! - `POST /api/storage/upload`, `GET /api/storage/{id}`: file storage (`crate::storage::http`).
//! - `/api/admin/*`: admin-only logs and stats (`crate::dashboard::http`); backups, export and import
//!   (`crate::ops::http`).
//! - `GET /metrics`: Prometheus metrics when enabled (`crate::ops::metrics`).
//!
//! `crate::ops::web::router` adds the app's own routes and static frontend around this router.
//! - `/_/`: the dashboard SPA (`crate::dashboard::assets`, feature `dashboard`; without it, a minimal `/_/setup`).

use std::future::Future;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;

use crate::error::ClientError;
use crate::runtime::App;
use crate::sync::protocol::ClientMessage;
use crate::sync::worker::{self, Inbound};

/// The axum router for `app`.
pub fn router(app: App) -> Router {
    Router::new()
        .route("/api/sync", get(sync_handler))
        .route("/api/query", post(http_query))
        .route("/api/mutation", post(http_mutation))
        .route("/api/action", post(http_action))
        .route("/api/health", get(|| async { axum::Json(json!({ "status": "ok" })) }))
        .merge(crate::auth::http::routes())
        .merge(crate::storage::http::routes())
        .merge(crate::dashboard::http::routes())
        .merge(crate::ops::http::routes())
        .merge(dashboard_ui())
        // Permissive CORS is fine: auth uses bearer tokens, never ambient cookies (the OAuth state cookie only rides
        // on top-level navigations, which CORS doesn't govern). Administrators can restrict origins at runtime.
        .layer(cors(&app))
        .with_state(app)
}

/// Any origin, unless the `corsOrigins` setting lists the allowed ones (`crate::settings`).
fn cors(app: &App) -> CorsLayer {
    let app = app.clone();
    CorsLayer::permissive().allow_origin(tower_http::cors::AllowOrigin::predicate(move |origin, _| {
        origin.to_str().is_ok_and(|o| app.auth().cors_allows(o))
    }))
}

#[cfg(feature = "dashboard")]
fn dashboard_ui() -> Router<App> {
    crate::dashboard::assets::routes()
}

#[cfg(not(feature = "dashboard"))]
fn dashboard_ui() -> Router<App> {
    crate::auth::http::setup_page_routes()
}

/// `listener`, setting `TCP_NODELAY` on every connection it accepts. Without it, Nagle's algorithm holds a small
/// write (a WebSocket update, a short response) until the peer acknowledges the previous one, and with delayed ACKs
/// that is up to ~40 ms: measured, update delivery had a p99 of 35 ms at a few hundred updates per second.
pub fn nodelay(listener: TcpListener) -> axum::serve::TapIo<TcpListener, fn(&mut tokio::net::TcpStream)> {
    use axum::serve::ListenerExt;
    fn set(stream: &mut tokio::net::TcpStream) {
        if let Err(e) = stream.set_nodelay(true) {
            tracing::debug!("could not set TCP_NODELAY: {e}");
        }
    }
    listener.tap_io(set as fn(&mut tokio::net::TcpStream))
}

/// Serve `app` on `listener` until the process ends.
pub async fn serve(listener: TcpListener, app: App) -> crate::Result<()> {
    axum::serve(nodelay(listener), router(app).into_make_service_with_connect_info::<std::net::SocketAddr>()).await?;
    Ok(())
}

/// Serve `app` on `listener` until `shutdown` resolves.
pub async fn serve_with_shutdown(
    listener: TcpListener,
    app: App,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> crate::Result<()> {
    axum::serve(nodelay(listener), router(app).into_make_service_with_connect_info::<std::net::SocketAddr>())
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------------
// WebSocket
// ---------------------------------------------------------------------------------------------------------------------

/// How long one WebSocket frame may take to send before the connection is given up.
pub const SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

async fn sync_handler(ws: WebSocketUpgrade, State(app): State<App>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(app, socket))
}

/// Wire a socket to a sync worker: a reader loop parses frames into the worker's input; a writer task serializes the
/// worker's output. When the worker stops (fatal error), the writer flushes, closes the socket, and we stop reading.
async fn handle_socket(app: App, socket: WebSocket) {
    struct Open(App);
    impl Drop for Open {
        fn drop(&mut self) {
            self.0.metrics().socket_closed();
        }
    }
    app.metrics().socket_opened();
    let _open = Open(app.clone());
    let (mut sink, mut stream) = socket.split();
    let (in_tx, mut out_rx) = worker::spawn(app);

    let mut writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            // A client that stops reading (while its TCP stack still acknowledges) would otherwise keep this task,
            // its worker and their queues alive for good.
            match tokio::time::timeout(SEND_TIMEOUT, sink.send(Message::Text(msg.to_text().into()))).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return,
                Err(_) => {
                    tracing::debug!("closing a WebSocket whose client stopped reading");
                    return;
                }
            }
        }
        let _ = tokio::time::timeout(SEND_TIMEOUT, sink.send(Message::Close(None))).await;
    });

    let reader = async {
        while let Some(frame) = stream.next().await {
            let inbound = match frame {
                Ok(Message::Text(text)) => match serde_json::from_str::<ClientMessage>(text.as_str()) {
                    Ok(m) => Inbound::Message(m),
                    Err(e) => Inbound::Malformed(e.to_string()),
                },
                Ok(Message::Binary(_)) => Inbound::Malformed("binary frames are not supported".into()),
                Ok(Message::Ping(_) | Message::Pong(_)) => continue,
                Ok(Message::Close(_)) | Err(_) => break,
            };
            let malformed = matches!(inbound, Inbound::Malformed(_));
            if in_tx.send(inbound).await.is_err() || malformed {
                break;
            }
        }
    };

    tokio::select! {
        _ = reader => {
            // Closing the input stops the worker, which drops its output and lets the writer finish.
            drop(in_tx);
            let _ = (&mut writer).await;
        }
        _ = &mut writer => {}
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// HTTP API
// ---------------------------------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct HttpCall {
    path: String,
    #[serde(default)]
    args: Value,
}

fn error_response(error: &ClientError, log_lines: Vec<String>) -> Response {
    let status = StatusCode::from_u16(error.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body =
        json!({ "status": "error", "errorMessage": error.message, "errorData": error.data, "logLines": log_lines });
    (status, axum::Json(body)).into_response()
}

fn client_log_lines(app: &App, lines: Vec<String>) -> Vec<String> {
    crate::log::for_client(app.options().dev, lines)
}

fn parse_call(body: &[u8]) -> Result<HttpCall, Box<Response>> {
    let mut call: HttpCall = serde_json::from_slice(body).map_err(|e| {
        let error = ClientError {
            kind: crate::error::ErrorKind::InvalidArgs,
            message: format!("Invalid request body: {e}"),
            data: None,
        };
        Box::new(error_response(&error, Vec::new()))
    })?;
    if call.args.is_null() {
        call.args = json!({});
    }
    Ok(call)
}

async fn http_query(State(app): State<App>, headers: HeaderMap, body: Bytes) -> Response {
    let call = match parse_call(&body) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let identity = match crate::auth::http::identity_from_headers(&app, &headers).await {
        Ok(i) => i,
        Err(e) => return e.into_response(),
    };
    let run = match app.query_or_cached(&call.path, call.args, &identity, false).await {
        // The cached result's JSON text goes into the response as is.
        crate::runtime::QueryOutcome::Cached(hit) => {
            let log_lines = serde_json::to_string(&hit.log_lines).unwrap_or_else(|_| "[]".into());
            let body = format!(r#"{{"status":"success","value":{},"logLines":{log_lines}}}"#, hit.value.json());
            return ([(axum::http::header::CONTENT_TYPE, "application/json")], body).into_response();
        }
        crate::runtime::QueryOutcome::Ran(run) => run,
    };
    let log_lines = client_log_lines(&app, run.log_lines);
    match run.result {
        Ok(value) => axum::Json(json!({ "status": "success", "value": value, "logLines": log_lines })).into_response(),
        Err(e) => error_response(&ClientError::from_anyhow(&e, app.options().dev), log_lines),
    }
}

async fn http_mutation(State(app): State<App>, headers: HeaderMap, body: Bytes) -> Response {
    let call = match parse_call(&body) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let identity = match crate::auth::http::identity_from_headers(&app, &headers).await {
        Ok(i) => i,
        Err(e) => return e.into_response(),
    };
    let run = app.execute_mutation(&call.path, call.args, &identity, None).await;
    let log_lines = client_log_lines(&app, run.log_lines);
    match run.result {
        Ok((value, ts)) => {
            axum::Json(json!({ "status": "success", "value": value, "ts": ts, "logLines": log_lines })).into_response()
        }
        Err(e) => error_response(&ClientError::from_anyhow(&e, app.options().dev), log_lines),
    }
}

async fn http_action(State(app): State<App>, headers: HeaderMap, body: Bytes) -> Response {
    let call = match parse_call(&body) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    let identity = match crate::auth::http::identity_from_headers(&app, &headers).await {
        Ok(i) => i,
        Err(e) => return e.into_response(),
    };
    let run = app.execute_action(&call.path, call.args, &identity).await;
    let log_lines = client_log_lines(&app, run.log_lines);
    match run.result {
        Ok(value) => axum::Json(json!({ "status": "success", "value": value, "logLines": log_lines })).into_response(),
        Err(e) => error_response(&ClientError::from_anyhow(&e, app.options().dev), log_lines),
    }
}
