//! den-mcp — Den's MCP server. An LLM client (Claude, ChatGPT) connected to Den Web's public origin asks it for
//! films, series and people; it asks den-atlas and answers with what a model may see and a link to Den Web.
//!
//!   POST /mcp                                     MCP (Streamable HTTP, JSON answers), bearer token required
//!   GET  /.well-known/oauth-protected-resource[/mcp]  RFC 9728: which authorization server issues its tokens
//!   GET  /health, /metrics
//!
//! den-edge is the authorization server, relays `/mcp` here, and signs the access tokens this server checks with
//! its public key alone. Discovery only: nothing here reads or writes a library.

mod atlas;
mod auth;
mod config;
mod mcp;
mod metrics;
mod names;
mod names_table;
mod sel;
mod tools;

#[cfg(test)]
mod tests;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{self, HeaderValue};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;

pub struct AppState {
    pub cfg: config::Config,
    pub atlas: atlas::Atlas,
    pub limit: auth::RateLimit,
    pub metrics: metrics::Metrics,
    /// Tool calls running at once, across every session: past it a call is told to come back, so a burst queues at
    /// the client rather than in this server's memory or at atlas.
    pub tool_slots: tokio::sync::Semaphore,
}

/// The most tool calls run at once. A call takes a few milliseconds of atlas's time, so this is far above any one
/// household's use and bounds what many sessions together can put on atlas.
pub const MAX_TOOL_CALLS: usize = 32;

impl AppState {
    pub fn new(cfg: config::Config) -> Self {
        let atlas = atlas::Atlas::new(cfg.atlas.base.clone(), cfg.atlas.timeout);
        let limit = auth::RateLimit::new(cfg.rate_per_minute, cfg.rate_burst);
        AppState {
            cfg,
            atlas,
            limit,
            metrics: metrics::Metrics::default(),
            tool_slots: tokio::sync::Semaphore::new(MAX_TOOL_CALLS),
        }
    }
}

/// A JSON-RPC message is small; this is room, not a target.
const MAX_BODY: usize = 64 * 1024;

pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The civil year of a unix time (Howard Hinnant's days-to-civil).
fn year_of(unix: u64) -> i64 {
    let z = (unix / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    yoe + era * 400 + i64::from(mp >= 10)
}

fn full(body: impl Into<Bytes>) -> Full<Bytes> {
    Full::new(body.into())
}

fn json_response(status: StatusCode, body: &Value) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, "no-store")
        .body(full(body.to_string()))
        .unwrap()
}

/// The caller's `X-Request-Id` as a log line may carry it: `[A-Za-z0-9_-]`, at most 32, the fleet's rule.
fn request_id(headers: &hyper::HeaderMap) -> Option<String> {
    let raw = headers.get("x-request-id")?.to_str().ok()?;
    let id: String =
        raw.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(32).collect();
    (!id.is_empty()).then_some(id)
}

/// Only with the configured token as `Authorization: Bearer <token>`, compared in constant time.
fn metrics_authorized(state: &AppState, headers: &hyper::HeaderMap) -> bool {
    let (Some(want), Some(given)) = (
        state.cfg.metrics_token.as_deref(),
        headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer ")),
    ) else {
        return false;
    };
    let (a, b) = (given.trim().as_bytes(), want.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub async fn handle<B>(state: Arc<AppState>, req: Request<B>) -> Response<Full<Bytes>>
where
    B: hyper::body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let started = Instant::now();
    let (parts, body) = req.into_parts();
    let rid = request_id(&parts.headers);
    let (resp, tool) = route(&state, &parts, body, rid.as_deref()).await;
    let route = match parts.uri.path() {
        p @ ("/mcp" | "/health" | "/metrics") => p,
        p if p.starts_with("/.well-known/") => "/.well-known",
        _ => "other",
    };
    state
        .metrics
        .count("mcp_requests_total", format!("route=\"{route}\",status=\"{}\"", resp.status().as_u16()));
    if state.cfg.log_requests {
        // The path and the tool's name, never its arguments: those are what someone asked.
        let mut line = format!(
            "{} {route} {} {}ms",
            parts.method,
            resp.status().as_u16(),
            started.elapsed().as_millis()
        );
        if let Some(tool) = tool {
            line.push_str(&format!(" tool={tool}"));
        }
        if let Some(rid) = rid {
            line.push_str(&format!(" rid={rid}"));
        }
        eprintln!("{line}");
    }
    resp
}

/// The answer, and the tool it ran if it ran one (for the log).
async fn route<B>(
    state: &AppState,
    parts: &hyper::http::request::Parts,
    body: B,
    rid: Option<&str>,
) -> (Response<Full<Bytes>>, Option<&'static str>)
where
    B: hyper::body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let get = matches!(parts.method, Method::GET | Method::HEAD);
    let resp = match parts.uri.path() {
        "/health" if get => json_response(StatusCode::OK, &health(state)),
        "/metrics" if get && metrics_authorized(state, &parts.headers) => Response::builder()
            .header(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")
            .header(header::CACHE_CONTROL, "no-store")
            .body(full(state.metrics.render(state.atlas.cached(), state.atlas.used())))
            .unwrap(),
        "/.well-known/oauth-protected-resource" | "/.well-known/oauth-protected-resource/mcp" if get => {
            json_response(StatusCode::OK, &resource_metadata(state))
        }
        "/mcp" if parts.method == Method::POST => return post_mcp(state, parts, body, rid).await,
        "/mcp" if parts.method == Method::OPTIONS => preflight(state, &parts.headers),
        "/mcp" => {
            // No server-initiated stream (GET) and no session to end (DELETE): the transport allows both answers.
            let mut resp =
                json_response(StatusCode::METHOD_NOT_ALLOWED, &json!({ "error": "method_not_allowed" }));
            resp.headers_mut().insert(header::ALLOW, HeaderValue::from_static("POST"));
            resp
        }
        _ => json_response(StatusCode::NOT_FOUND, &json!({ "error": "not_found" })),
    };
    (resp, None)
}

fn health(state: &AppState) -> Value {
    if state.cfg.token_keys.is_empty() {
        json!({ "status": "degraded", "reason": "no_token_keys",
                "detail": "TOKEN_PUBLIC_KEYS is unset, so every call to /mcp is refused" })
    } else {
        json!({ "status": "ok" })
    }
}

/// RFC 9728: this resource, and den-edge as the authorization server that issues its tokens.
fn resource_metadata(state: &AppState) -> Value {
    json!({
        "resource": state.cfg.resource(),
        "authorization_servers": [state.cfg.issuer],
        "scopes_supported": [auth::SCOPE],
        "bearer_methods_supported": ["header"],
        "resource_name": "Den",
    })
}

/// The request's `Origin`, when it is one a browser-based client may call from. A request with none — a
/// connector's own server — passes; one naming any other site is refused.
fn origin_allowed(state: &AppState, headers: &hyper::HeaderMap) -> Result<Option<HeaderValue>, ()> {
    let Some(origin) = headers.get(header::ORIGIN) else { return Ok(None) };
    let value = origin.to_str().map_err(|_| ())?.to_ascii_lowercase();
    if value == state.cfg.public_origin || state.cfg.allowed_origins.contains(&value) {
        Ok(Some(origin.clone()))
    } else {
        Err(())
    }
}

fn preflight(state: &AppState, headers: &hyper::HeaderMap) -> Response<Full<Bytes>> {
    let Ok(Some(origin)) = origin_allowed(state, headers) else {
        return json_response(StatusCode::FORBIDDEN, &json!({ "error": "origin_not_allowed" }));
    };
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin)
        .header(header::ACCESS_CONTROL_ALLOW_METHODS, "POST")
        .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "authorization, content-type, mcp-protocol-version")
        .header(header::ACCESS_CONTROL_MAX_AGE, "86400")
        .body(full(""))
        .unwrap()
}

/// 401, and where to find out how to get a token (the MCP authorization spec's `WWW-Authenticate`).
fn unauthorized(state: &AppState, refused: &auth::Refused) -> Response<Full<Bytes>> {
    let mut challenge =
        format!("Bearer realm=\"den\", resource_metadata=\"{}\"", state.cfg.resource_metadata_url());
    if let auth::Refused::Invalid(why) = refused {
        challenge.push_str(&format!(", error=\"invalid_token\", error_description=\"{why}\""));
    }
    let mut resp = json_response(StatusCode::UNAUTHORIZED, &json!({ "error": "unauthorized" }));
    if let Ok(value) = HeaderValue::from_str(&challenge) {
        resp.headers_mut().insert(header::WWW_AUTHENTICATE, value);
    }
    resp
}

async fn post_mcp<B>(
    state: &AppState,
    parts: &hyper::http::request::Parts,
    body: B,
    rid: Option<&str>,
) -> (Response<Full<Bytes>>, Option<&'static str>)
where
    B: hyper::body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let cors = match origin_allowed(state, &parts.headers) {
        Ok(cors) => cors,
        Err(()) => {
            return (json_response(StatusCode::FORBIDDEN, &json!({ "error": "origin_not_allowed" })), None)
        }
    };
    let authorization = parts.headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    let caller = match auth::verify(
        authorization,
        &state.cfg.token_keys,
        &state.cfg.issuer,
        &state.cfg.resource(),
        unix_now(),
    ) {
        Ok(caller) => caller,
        Err(refused) => {
            let why = match &refused {
                auth::Refused::Missing => "missing",
                auth::Refused::Invalid(why) => why,
            };
            state.metrics.count("mcp_auth_refused_total", format!("reason=\"{why}\""));
            return (unauthorized(state, &refused), None);
        }
    };
    if let Err(wait) = state.limit.take(&caller.session, Instant::now()) {
        state.metrics.count("mcp_rate_limited_total", String::new());
        let mut resp = json_response(StatusCode::TOO_MANY_REQUESTS, &json!({ "error": "rate_limited" }));
        resp.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(wait));
        return (resp, None);
    }
    let version = parts.headers.get("mcp-protocol-version").and_then(|v| v.to_str().ok());
    if version.is_some_and(|v| !mcp::VERSIONS.contains(&v)) {
        return (
            json_response(StatusCode::BAD_REQUEST, &json!({ "error": "unsupported_protocol_version" })),
            None,
        );
    }
    let Ok(body) = Limited::new(body, MAX_BODY).collect().await.map(|c| c.to_bytes()) else {
        return (
            json_response(StatusCode::PAYLOAD_TOO_LARGE, &json!({ "error": "payload_too_large" })),
            None,
        );
    };
    let (answer, tool) = match mcp::read(&body, version.unwrap_or(mcp::DEFAULT_VERSION)) {
        Err(error) => (Some(error), None),
        Ok(mcp::Body::One(message)) => reply(state, &caller, message, rid).await,
        Ok(mcp::Body::Batch(members)) => {
            // Each member answered in turn; notifications add nothing, and a batch of only those is a 202.
            let mut replies = Vec::new();
            let mut first_tool = None;
            for member in members {
                let (answer, tool) = match member {
                    Err(error) => (Some(error), None),
                    Ok(message) => reply(state, &caller, message, rid).await,
                };
                replies.extend(answer);
                first_tool = first_tool.or(tool);
            }
            ((!replies.is_empty()).then_some(Value::Array(replies)), first_tool)
        }
    };
    let mut resp = match answer {
        Some(message) => json_response(StatusCode::OK, &message),
        None => Response::builder().status(StatusCode::ACCEPTED).body(full("")).unwrap(),
    };
    if let Some(origin) = cors {
        resp.headers_mut().insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    }
    (resp, tool)
}

async fn reply(
    state: &AppState,
    caller: &auth::Caller,
    message: mcp::Message,
    rid: Option<&str>,
) -> (Option<Value>, Option<&'static str>) {
    match message {
        mcp::Message::NoReply => (None, None),
        mcp::Message::Request { id, method, params } => {
            answer(state, caller, id, &method, &params, rid).await
        }
    }
}

/// The tools' names as `&'static str`, for the log and the metrics labels: never a name a client made up.
fn known_tool(name: &str) -> Option<&'static str> {
    [
        "den_search",
        "den_filter_titles",
        "den_filter_values",
        "den_find_people",
        "den_title",
        "den_similar",
        "search",
        "fetch",
    ]
    .into_iter()
    .find(|t| *t == name)
}

async fn answer(
    state: &AppState,
    caller: &auth::Caller,
    id: Value,
    method: &str,
    params: &Value,
    rid: Option<&str>,
) -> (Option<Value>, Option<&'static str>) {
    let reply = match method {
        "initialize" => mcp::result(id, mcp::initialize(params)),
        "ping" => mcp::result(id, json!({})),
        "tools/list" => mcp::result(id, json!({ "tools": tools::list() })),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let Some(tool) = known_tool(name) else {
                return (Some(mcp::error(id, mcp::INVALID_PARAMS, &format!("Unknown tool: {name}"))), None);
            };
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let Ok(_slot) = state.tool_slots.try_acquire() else {
                state.metrics.count("mcp_busy_total", String::new());
                let busy = Err(tools::ToolError("Den is busy; try again in a few seconds.".into()));
                return (Some(mcp::result(id, mcp::tool_result(busy))), Some(tool));
            };
            let started = Instant::now();
            let tmdb_kinds = state.atlas.tmdb_kinds(rid).await;
            let ctx = tools::Ctx {
                atlas: &state.atlas,
                cfg: &state.cfg,
                rid,
                year: year_of(unix_now()),
                tmdb_kinds: &tmdb_kinds,
            };
            let answer = tools::call(&ctx, tool, &args).await.expect("a known tool runs");
            state.metrics.time("mcp_tool_seconds_total", started.elapsed().as_micros() as u64);
            let outcome = if answer.is_ok() { "ok" } else { "error" };
            state.metrics.count(
                "mcp_tool_calls_total",
                format!("tool=\"{tool}\",outcome=\"{outcome}\",caller=\"{}\"", caller.kind),
            );
            return (Some(mcp::result(id, mcp::tool_result(answer))), Some(tool));
        }
        other => mcp::error(id, mcp::METHOD_NOT_FOUND, &format!("Method not found: {other}")),
    };
    (Some(reply), None)
}

/// How long in-flight requests get to finish after SIGTERM: under podman's default 10 s stop timeout.
const DRAIN_GRACE: Duration = Duration::from_secs(8);
/// How long a client may take to send a request head.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Serve until `shutdown` resolves, then let in-flight requests finish for at most `grace`; `true` when they did.
pub async fn serve_until(
    listener: TcpListener,
    state: Arc<AppState>,
    shutdown: impl Future<Output = ()>,
    grace: Duration,
) -> bool {
    let graceful = GracefulShutdown::new();
    tokio::pin!(shutdown);
    loop {
        let (stream, _) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(pair) => pair,
                Err(e) => {
                    eprintln!("accept: {e}");
                    // The listener stays readable while the process is out of descriptors; back off rather than
                    // spin the one runtime thread.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
            _ = &mut shutdown => break,
        };
        let _ = stream.set_nodelay(true);
        let state = state.clone();
        let service = service_fn(move |req: Request<Incoming>| {
            let state = state.clone();
            async move { Ok::<_, Infallible>(handle(state, req).await) }
        });
        let conn = hyper::server::conn::http1::Builder::new()
            .timer(TokioTimer::new())
            .header_read_timeout(HEADER_READ_TIMEOUT)
            .serve_connection(TokioIo::new(stream), service);
        let conn = graceful.watch(conn);
        tokio::spawn(async move {
            let _ = conn.await;
        });
    }
    drop(listener);
    tokio::select! {
        _ = graceful.shutdown() => true,
        _ = tokio::time::sleep(grace) => {
            eprintln!("drain deadline ({grace:?}) reached with requests still in flight");
            false
        }
    }
}

/// Resolves on SIGTERM (a redeploy) or SIGINT (a terminal); a second signal exits at once. PID 1 in a `scratch`
/// image gets no default terminate action, so without this every restart waits out the stop timeout.
fn shutdown_signal() -> impl Future<Output = ()> {
    use tokio::signal::unix::{signal, SignalKind};
    // Registered now, not when first polled, so a stop that arrives before the server polls it still drains.
    let term = signal(SignalKind::terminate());
    let int = signal(SignalKind::interrupt());
    async move {
        tokio::select! {
            _ = wait_for(term, "SIGTERM") => {}
            _ = wait_for(int, "SIGINT") => {}
        }
        tokio::spawn(async move {
            tokio::select! {
                _ = wait_for(signal(SignalKind::terminate()), "") => {}
                _ = wait_for(signal(SignalKind::interrupt()), "") => {}
            }
            eprintln!("second signal — exiting without finishing the drain");
            std::process::exit(0);
        });
    }
}

async fn wait_for(registered: std::io::Result<tokio::signal::unix::Signal>, name: &str) {
    match registered {
        Ok(mut sig) => {
            sig.recv().await;
            if !name.is_empty() {
                eprintln!("{name} — draining in-flight requests");
            }
        }
        Err(e) => {
            eprintln!("signal handler unavailable ({e}); a stop will be a hard kill");
            std::future::pending::<()>().await
        }
    }
}

async fn run(cfg: config::Config) -> std::io::Result<()> {
    let shutdown = shutdown_signal();
    let listener = TcpListener::bind(("0.0.0.0", cfg.port)).await?;
    let state = Arc::new(AppState::new(cfg));
    let cfg = &state.cfg;
    eprintln!(
        "den-mcp {} listening on :{} — atlas={} origin={} issuer={} token_keys={} rate={}/min burst={} metrics={} \
         log_requests={}",
        env!("CARGO_PKG_VERSION"),
        listener.local_addr().map_or(cfg.port, |a| a.port()),
        cfg.atlas.base,
        cfg.public_origin,
        cfg.issuer,
        cfg.token_keys.len(),
        cfg.rate_per_minute,
        cfg.rate_burst,
        if cfg.metrics_token.is_some() { "on" } else { "off" },
        if cfg.log_requests { "on" } else { "off" },
    );
    if cfg.token_keys.is_empty() {
        eprintln!(
            "health: degraded (no_token_keys) — set TOKEN_PUBLIC_KEYS, or every call to /mcp is refused"
        );
    }
    if serve_until(listener, state, shutdown, DRAIN_GRACE).await {
        eprintln!("shut down cleanly");
    }
    Ok(())
}

fn main() {
    let cfg = match config::Config::from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("config: {e}");
            std::process::exit(1);
        }
    };
    // current_thread: one runtime thread keeps idle RAM low; each request is a short relay to atlas.
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    if let Err(e) = rt.block_on(run(cfg)) {
        eprintln!("fatal: {e}");
        std::process::exit(1);
    }
}
