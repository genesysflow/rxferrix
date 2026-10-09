//! HTTP + WebSocket server (axum).
//!
//! - `GET  /api/sync`: the WebSocket sync protocol (docs/PROTOCOL.md).
//! - `POST /api/query | /api/mutation | /api/action`: the HTTP fallback, `{ "path", "args" }`.
//! - `GET  /api/health`.

use std::future::Future;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;

use crate::error::ClientError;
use crate::runtime::{App, Identity};
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
        // Phase 0: permissive CORS in every mode (single-user apps, no cookies). Tighten once auth lands.
        .layer(CorsLayer::permissive())
        .with_state(app)
}

/// Serve `app` on `listener` until the process ends.
pub async fn serve(listener: TcpListener, app: App) -> crate::Result<()> {
    axum::serve(listener, router(app)).await?;
    Ok(())
}

/// Serve `app` on `listener` until `shutdown` resolves.
pub async fn serve_with_shutdown(
    listener: TcpListener,
    app: App,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> crate::Result<()> {
    axum::serve(listener, router(app)).with_graceful_shutdown(shutdown).await?;
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

fn error_response(error: &ClientError) -> Response {
    let status = StatusCode::from_u16(error.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = json!({ "status": "error", "errorMessage": error.message, "errorData": error.data, "logLines": [] });
    (status, axum::Json(body)).into_response()
}

fn parse_call(body: &[u8]) -> Result<HttpCall, Box<Response>> {
    let mut call: HttpCall = serde_json::from_slice(body).map_err(|e| {
        let error = ClientError {
            kind: crate::error::ErrorKind::InvalidArgs,
            message: format!("Invalid request body: {e}"),
            data: None,
        };
        Box::new(error_response(&error))
    })?;
    if call.args.is_null() {
        call.args = json!({});
    }
    Ok(call)
}

async fn http_query(State(app): State<App>, body: Bytes) -> Response {
    let call = match parse_call(&body) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    // Phase 0: the Authorization header is accepted but ignored.
    match app.run_query(&call.path, call.args, &Identity::default()).await.result {
        Ok(value) => axum::Json(json!({ "status": "success", "value": value, "logLines": [] })).into_response(),
        Err(e) => error_response(&ClientError::from_anyhow(&e, app.options().dev)),
    }
}

async fn http_mutation(State(app): State<App>, body: Bytes) -> Response {
    let call = match parse_call(&body) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    match app.run_mutation(&call.path, call.args, &Identity::default()).await {
        Ok((value, ts)) => {
            axum::Json(json!({ "status": "success", "value": value, "ts": ts, "logLines": [] })).into_response()
        }
        Err(e) => error_response(&ClientError::from_anyhow(&e, app.options().dev)),
    }
}

async fn http_action(State(app): State<App>, body: Bytes) -> Response {
    let call = match parse_call(&body) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    match app.run_action(&call.path, call.args, &Identity::default()).await {
        Ok(value) => axum::Json(json!({ "status": "success", "value": value, "logLines": [] })).into_response(),
        Err(e) => error_response(&ClientError::from_anyhow(&e, app.options().dev)),
    }
}
