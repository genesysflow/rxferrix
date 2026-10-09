//! HTTP + WebSocket server (axum).
//!
//! - `GET  /api/sync`: the WebSocket sync protocol (docs/PROTOCOL.md).
//! - `POST /api/query | /api/mutation | /api/action`: the HTTP fallback, `{ "path", "args" }`.
//! - `GET  /api/health`.
//! - `/api/auth/*` and `/_/setup`: authentication (`crate::auth::http`).

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
        // Permissive CORS is fine: auth uses bearer tokens, never ambient cookies (the OAuth state cookie only rides
        // on top-level navigations, which CORS doesn't govern).
        .layer(CorsLayer::permissive())
        .with_state(app)
}

/// Serve `app` on `listener` until the process ends.
pub async fn serve(listener: TcpListener, app: App) -> crate::Result<()> {
    axum::serve(listener, router(app).into_make_service_with_connect_info::<std::net::SocketAddr>()).await?;
    Ok(())
}

/// Serve `app` on `listener` until `shutdown` resolves.
pub async fn serve_with_shutdown(
    listener: TcpListener,
    app: App,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> crate::Result<()> {
    axum::serve(listener, router(app).into_make_service_with_connect_info::<std::net::SocketAddr>())
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------------
// WebSocket
// ---------------------------------------------------------------------------------------------------------------------

async fn sync_handler(ws: WebSocketUpgrade, State(app): State<App>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(app, socket))
}

/// Wire a socket to a sync worker: a reader loop parses frames into the worker's input; a writer task serializes the
/// worker's output. When the worker stops (fatal error), the writer flushes, closes the socket, and we stop reading.
async fn handle_socket(app: App, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (in_tx, mut out_rx) = worker::spawn(app);

    let mut writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if sink.send(Message::Text(msg.to_text().into())).await.is_err() {
                return;
            }
        }
        let _ = sink.send(Message::Close(None)).await;
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
    let run = app.run_query(&call.path, call.args, &identity).await;
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
