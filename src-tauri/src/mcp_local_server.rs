//! Loopback MCP HTTP endpoint for self-hosted automation.
//!
//! Upstream serves MCP through the cloud remote bridge only. This module
//! keeps the fork's local listener, so agents on this machine can drive the
//! same engine without a Donut cloud account. Every request is forwarded to
//! [`McpServer::handle_message`], which means the loopback surface stays in
//! lockstep with the remote bridge: same tools, same toolsets, same gating,
//! same rate limits. Only the transport differs.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex as StdMutex;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, Request as HttpRequest, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::mcp_server::{McpOutcome, McpServer};
use crate::settings_manager::SettingsManager;

pub const DEFAULT_MCP_LOCAL_PORT: u16 = 51080;

#[derive(Clone)]
struct McpLocalState {
  token: std::sync::Arc<String>,
}

static LOCAL_RUNNING: AtomicBool = AtomicBool::new(false);
static LOCAL_SHUTDOWN: StdMutex<Option<oneshot::Sender<()>>> = StdMutex::new(None);

/// Largest JSON-RPC payload the loopback endpoint accepts; matches the
/// engine's own `MAX_MESSAGE_BYTES`.
const MAX_LOCAL_MESSAGE_BYTES: usize = 1024 * 1024;

/// Start the loopback MCP server. Returns the port it bound.
///
/// Idempotent at the caller's level: starting twice is an error, and the
/// settings toggle that reaches here is the same one `save_settings` uses to
/// mint the token this server authenticates with.
pub async fn start(app_handle: tauri::AppHandle) -> Result<u16, String> {
  if LOCAL_RUNNING.load(Ordering::SeqCst) {
    return Err(crate::backend_error("MCP_SERVER_ALREADY_RUNNING"));
  }

  let settings_manager = SettingsManager::instance();
  let settings = settings_manager
    .load_settings()
    .map_err(|e| crate::backend_error_with_detail("INTERNAL_ERROR", e))?;

  let token = if let Some(t) = settings_manager
    .get_mcp_token(&app_handle)
    .await
    .ok()
    .flatten()
  {
    t
  } else {
    settings_manager
      .generate_mcp_token(&app_handle)
      .await
      .map_err(|e| crate::backend_error_with_detail("INTERNAL_ERROR", e))?
  };

  let preferred_port = settings.mcp_port.unwrap_or(DEFAULT_MCP_LOCAL_PORT);
  let listener = bind_to_available_port(preferred_port).await?;
  let actual_port = listener
    .local_addr()
    .map_err(|e| crate::backend_error_with_detail("INTERNAL_ERROR", e))?
    .port();

  let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
  {
    let mut guard = LOCAL_SHUTDOWN
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = Some(shutdown_tx);
  }

  let state = McpLocalState {
    token: std::sync::Arc::new(token),
  };

  LOCAL_RUNNING.store(true, Ordering::SeqCst);
  tokio::spawn(run_http_server(listener, state, shutdown_rx));

  log::info!("[mcp] Local MCP server started on port {}", actual_port);
  Ok(actual_port)
}

/// Stop the loopback MCP server. Stopping a server that is not running is a
/// no-op, so the settings toggle and app teardown can both call it.
pub async fn stop() -> Result<(), String> {
  let shutdown_tx = {
    let mut guard = LOCAL_SHUTDOWN
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.take()
  };
  if let Some(shutdown_tx) = shutdown_tx {
    let _ = shutdown_tx.send(());
    log::info!("[mcp] Local MCP server stopped");
  }
  LOCAL_RUNNING.store(false, Ordering::SeqCst);
  Ok(())
}

/// Whether the loopback MCP server is currently listening.
pub fn is_running() -> bool {
  LOCAL_RUNNING.load(Ordering::SeqCst)
}

async fn bind_to_available_port(preferred: u16) -> Result<TcpListener, String> {
  let addr = SocketAddr::from(([127, 0, 0, 1], preferred));
  if let Ok(listener) = TcpListener::bind(addr).await {
    return Ok(listener);
  }

  for _ in 0..10 {
    let port = 51000 + (rand::random::<u16>() % 1000);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    if let Ok(listener) = TcpListener::bind(addr).await {
      return Ok(listener);
    }
  }

  Err(crate::backend_error("MCP_PORT_UNAVAILABLE"))
}

async fn run_http_server(
  listener: TcpListener,
  state: McpLocalState,
  shutdown_rx: oneshot::Receiver<()>,
) {
  let app = Router::new()
    .route(
      "/mcp/{token}",
      post(handle_mcp_post)
        .get(method_not_allowed)
        .delete(handle_mcp_delete),
    )
    .route(
      "/mcp",
      post(handle_mcp_post)
        .get(method_not_allowed)
        .delete(handle_mcp_delete),
    )
    .route("/health", get(handle_health))
    .layer(axum::middleware::from_fn_with_state(
      state.clone(),
      auth_middleware,
    ))
    .with_state(state);

  let port = listener.local_addr().map(|addr| addr.port()).unwrap_or(0);
  let server = async move {
    log::info!(
      "[mcp] Local server listening on http://127.0.0.1:{}/mcp",
      port
    );
    if let Err(e) = axum::serve(listener, app).await {
      log::error!("[mcp] Local server error: {}", e);
    }
  };

  tokio::select! {
    _ = server => {},
    _ = shutdown_rx => {
      log::info!("[mcp] Local server shutting down");
    },
  }
}

async fn method_not_allowed() -> StatusCode {
  // Loopback parity with the remote bridge: no server-initiated SSE streams.
  StatusCode::METHOD_NOT_ALLOWED
}

async fn auth_middleware(
  State(state): State<McpLocalState>,
  req: HttpRequest<Body>,
  next: Next,
) -> Result<Response, StatusCode> {
  let path = req.uri().path();

  if path == "/health" {
    return Ok(next.run(req).await);
  }

  let path_token = path
    .strip_prefix("/mcp/")
    .filter(|t| !t.is_empty() && !t.contains('/'));

  let header_token = req
    .headers()
    .get(header::AUTHORIZATION)
    .and_then(|h| h.to_str().ok())
    .and_then(|h| h.strip_prefix("Bearer "));

  // Constant-time comparison to avoid leaking the token prefix via timing.
  use subtle::ConstantTimeEq;
  let expected = state.token.as_bytes();
  let ct_eq = |t: Option<&str>| {
    t.is_some_and(|t| {
      let b = t.as_bytes();
      b.len() == expected.len() && b.ct_eq(expected).into()
    })
  };
  let valid = ct_eq(path_token) || ct_eq(header_token);

  if !valid {
    return Err(StatusCode::UNAUTHORIZED);
  }

  Ok(next.run(req).await)
}

async fn handle_health() -> impl IntoResponse {
  Json(serde_json::json!({
    "status": "ok",
    "server": "donut-browser",
  }))
}

async fn handle_mcp_delete(req: HttpRequest<Body>) -> Response {
  let session_id = req
    .headers()
    .get("mcp-session-id")
    .and_then(|h| h.to_str().ok())
    .map(|s| s.to_string());

  if let Some(sid) = session_id {
    McpServer::instance().end_session(&sid).await;
  }

  StatusCode::OK.into_response()
}

async fn handle_mcp_post(req: Request) -> Response {
  let session_id = req
    .headers()
    .get("mcp-session-id")
    .and_then(|h| h.to_str().ok())
    .map(|s| s.to_string());

  let body_bytes = match axum::body::to_bytes(req.into_body(), MAX_LOCAL_MESSAGE_BYTES).await {
    Ok(b) => b,
    Err(_) => {
      return (StatusCode::BAD_REQUEST, "Invalid request body").into_response();
    }
  };

  let outcome = McpServer::instance()
    .handle_message_with(session_id.as_deref(), &body_bytes, None)
    .await;

  match outcome {
    McpOutcome::Body {
      body,
      new_session_id,
    } => {
      let mut response = (StatusCode::OK, Json(body)).into_response();
      if let Some(sid) = new_session_id {
        if let Ok(value) = HeaderValue::from_str(&sid) {
          response.headers_mut().insert("mcp-session-id", value);
        }
      }
      response
    }
    McpOutcome::Accepted => StatusCode::ACCEPTED.into_response(),
    McpOutcome::UnknownSession => StatusCode::NOT_FOUND.into_response(),
    McpOutcome::BadRequest => StatusCode::BAD_REQUEST.into_response(),
    McpOutcome::RateLimited { retry_after_secs } => {
      let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        "automation rate limit exceeded",
      )
        .into_response();
      if let Ok(value) = HeaderValue::from_str(&retry_after_secs.to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
      }
      response
    }
  }
}
