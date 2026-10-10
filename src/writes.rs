//! Library writes (den-spec `wire/assistant-v1.md`): an assistant changes the watchlist, marks something seen or
//! rates it. This server never touches a library; it **seals** a request to the library's drop-box key, signed by the
//! grant the household gave this connection, and hands it to den-edge's queue. A TV or Den Web drains the queue,
//! checks every request and applies it through the same path as a tap.
//!
//! The grant key reaches this server inside the access token (`dw`, sealed to `MCP_WRITE_KEY`), is opened for one
//! call and dropped with it: it is never stored. All crypto is den-core's `den-assistant`.

use crate::auth::Caller;
use crate::config::Write;
use crate::tools::{bad, title_key, ToolError};
use bytes::Bytes;
use den_assistant::{
    b64url_decode, hex, key_id, open_claim, seal_request, Request, ESEED_LEN, KEM_PUBLIC_LEN, MAX_EPISODE,
    MAX_SAFE_INTEGER,
};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{header, Request as HttpRequest, StatusCode};
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How long a library's drop-box key is reused for a session: an assistant often makes several changes in a row.
const DROPBOX_TTL: Duration = Duration::from_secs(60);
/// The most sessions whose drop-box key is kept.
const DROPBOXES_KEPT: usize = 256;
/// A call to den-edge, the wait for its answer included.
const EDGE_TIMEOUT: Duration = Duration::from_secs(10);
/// den-edge's answers here are a few hundred bytes.
const MAX_ANSWER: usize = 16 * 1024;

/// Said with every queued change: it has not happened yet.
const QUEUED_NOTE: &str =
    "Den applies it next time a TV or Den Web is open; usually within seconds when the TV is on.";

/// The four tools, as `tools/list` names them, and the op each sends.
pub const TOOLS: [(&str, &str); 4] = [
    ("den_watchlist_add", "watchlist_add"),
    ("den_watchlist_remove", "watchlist_remove"),
    ("den_mark_seen", "seen"),
    ("den_rate", "rate"),
];

#[derive(Clone)]
struct Dropbox {
    library: String,
    public: Vec<u8>,
}

pub struct Writes {
    secret: [u8; 32],
    edge: String,
    client: Client<HttpConnector, Full<Bytes>>,
    dropboxes: Mutex<HashMap<String, (Dropbox, Instant)>>,
}

/// Why a change was not queued: a short code for the log and the words the model is told.
struct Refused(&'static str, String);

fn refuse(code: &'static str, message: &str) -> Refused {
    Refused(code, message.to_owned())
}

impl Writes {
    pub fn new(config: &Write) -> Self {
        let mut connector = HttpConnector::new();
        connector.set_nodelay(true);
        Writes {
            secret: config.key,
            edge: config.edge.clone(),
            client: Client::builder(TokioExecutor::new()).build(connector),
            dropboxes: Mutex::default(),
        }
    }

    /// Whether the caller's token carries a grant key this server opens for the token's session: the write tools are
    /// listed for no other.
    pub fn can_write(&self, caller: &Caller) -> bool {
        caller.write.as_ref().is_some_and(|w| open_claim(&self.secret, &caller.session, &w.claim).is_ok())
    }

    /// One write tool call: its arguments checked, the change sealed and queued at den-edge. `log` says whether to
    /// print the outcome — the tool and a status word, never the arguments or a key.
    pub async fn call(
        &self,
        caller: &Caller,
        tool: &str,
        args: &Value,
        log: bool,
    ) -> Result<Value, ToolError> {
        let op = TOOLS.iter().find(|(name, _)| *name == tool).map_or("", |(_, op)| op);
        let args = build_args(op, args)?;
        let outcome = self.queue(caller, op, args).await;
        let status = match &outcome {
            Ok(_) => "queued",
            Err(Refused(code, _)) => code,
        };
        if log {
            eprintln!("write tool={tool} status={status}");
        }
        outcome.map_err(|Refused(_, message)| ToolError(message))
    }

    async fn queue(&self, caller: &Caller, op: &str, args: Value) -> Result<Value, Refused> {
        let denied = || {
            refuse(
                "no_write_grant",
                "This connection can't change your library — reconnect and allow changes.",
            )
        };
        let grant = caller.write.as_ref().ok_or_else(denied)?;
        let key = open_claim(&self.secret, &caller.session, &grant.claim).map_err(|_| denied())?;
        let dropbox = self.dropbox(&caller.session, &grant.token).await?;
        let id = hex(&random::<16>()?);
        let at = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let request = Request { library: &dropbox.library, grant: key.id(), id: &id, at, op, args: &args };
        let sealed = seal_request(&dropbox.public, &key, &request, &random::<ESEED_LEN>()?)
            .map_err(|_| refuse("not_sealed", "Den could not prepare that change."))?;
        drop(key);
        let (status, answer) = self
            .edge_call("POST", "/assistant/append", &grant.token, Some(json!({ "sealed": sealed })))
            .await?;
        match (status.as_u16(), error_code(&answer)) {
            (200, _) => Ok(json!({ "queued": true, "id": id, "note": QUEUED_NOTE })),
            (401, _) => Err(refuse("unauthorized", "This connection has expired — reconnect Den and try again.")),
            (403, _) => Err(denied()),
            (409, _) => Err(refuse(
                "queue_full",
                "Den already has 200 changes waiting to be applied. They are applied when a TV or Den Web is \
                 open; try again after that.",
            )),
            (429, _) => Err(refuse(
                "rate_limited",
                "Too many changes in a short while; wait a minute and try again.",
            )),
            (status, code) => {
                eprintln!("den-edge: append answered {status} {}", code.unwrap_or("-"));
                Err(refuse("edge_error", "Den could not queue that change right now; try again later."))
            }
        }
    }

    /// The session's library and its drop-box public key, from den-edge, kept for a minute.
    async fn dropbox(&self, session: &str, token: &str) -> Result<Dropbox, Refused> {
        if let Some((dropbox, at)) = self.dropboxes.lock().unwrap_or_else(|e| e.into_inner()).get(session) {
            if at.elapsed() < DROPBOX_TTL {
                return Ok(dropbox.clone());
            }
        }
        let (status, answer) = self.edge_call("GET", "/assistant/dropbox", token, None).await?;
        let dropbox = match status.as_u16() {
            200 => parse_dropbox(&answer).ok_or_else(|| {
                refuse("edge_error", "Den could not queue that change right now; try again later.")
            })?,
            404 => return Err(refuse(
                "no_dropbox",
                "Your library isn't ready for changes from an assistant yet — open Den on the TV or in Den \
                     Web once, then try again.",
            )),
            401 => {
                return Err(refuse(
                    "unauthorized",
                    "This connection has expired — reconnect Den and try again.",
                ))
            }
            403 => {
                return Err(refuse(
                    "no_write_grant",
                    "This connection can't change your library — reconnect and allow changes.",
                ))
            }
            other => {
                eprintln!("den-edge: dropbox answered {other} {}", error_code(&answer).unwrap_or("-"));
                return Err(refuse(
                    "edge_error",
                    "Den could not queue that change right now; try again later.",
                ));
            }
        };
        let mut kept = self.dropboxes.lock().unwrap_or_else(|e| e.into_inner());
        if kept.len() >= DROPBOXES_KEPT {
            kept.retain(|_, (_, at)| at.elapsed() < DROPBOX_TTL);
            if kept.len() >= DROPBOXES_KEPT {
                kept.clear();
            }
        }
        kept.insert(session.to_owned(), (dropbox.clone(), Instant::now()));
        Ok(dropbox)
    }

    /// One JSON exchange with den-edge as the caller, the access token as its bearer.
    async fn edge_call(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<Value>,
    ) -> Result<(StatusCode, Value), Refused> {
        let unreachable = |why: String| {
            eprintln!("den-edge: {why}");
            refuse("edge_unreachable", "Den could not reach its sync service; try again later.")
        };
        let mut req = HttpRequest::builder()
            .method(method)
            .uri(format!("{}{path}", self.edge))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::ACCEPT, "application/json");
        let body = match body {
            Some(body) => {
                req = req.header(header::CONTENT_TYPE, "application/json");
                Bytes::from(body.to_string())
            }
            None => Bytes::new(),
        };
        let req = req.body(Full::new(body)).map_err(|e| unreachable(e.to_string()))?;
        let exchange = async {
            let resp = self.client.request(req).await.map_err(|e| format!("unreachable: {e}"))?;
            let (parts, body) = resp.into_parts();
            let body =
                Limited::new(body, MAX_ANSWER).collect().await.map_err(|e| format!("unreadable: {e}"))?;
            Ok::<_, String>((parts.status, body.to_bytes()))
        };
        let (status, body) = tokio::time::timeout(EDGE_TIMEOUT, exchange)
            .await
            .map_err(|_| unreachable("no answer in time".into()))?
            .map_err(unreachable)?;
        Ok((status, serde_json::from_slice(&body).unwrap_or(Value::Null)))
    }
}

/// den-edge's `{"error": "<code>"}`.
fn error_code(answer: &Value) -> Option<&str> {
    answer.get("error").and_then(Value::as_str)
}

/// `{"library", "pk", "kid"}`, checked: a library id, a key of the right length, and the id that key has.
fn parse_dropbox(answer: &Value) -> Option<Dropbox> {
    let library = answer.get("library")?.as_str()?;
    let public = b64url_decode(answer.get("pk")?.as_str()?)?;
    let well_formed = library.len() == 32 && library.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    (well_formed && public.len() == KEM_PUBLIC_LEN && answer.get("kid")?.as_str()? == key_id(&public))
        .then(|| Dropbox { library: library.to_owned(), public })
}

/// `N` bytes from the platform's CSPRNG.
fn random<const N: usize>() -> Result<[u8; N], Refused> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|e| {
        eprintln!("randomness unavailable: {e}");
        refuse("no_randomness", "Den could not prepare that change.")
    })?;
    Ok(bytes)
}

/// A tool's arguments as the op's `args`: the title as library rows key it (`series` is the library's `tv`) and the
/// rest checked to the spec's limits, so a mistake is said to the model rather than refused at a device.
fn build_args(op: &str, args: &Value) -> Result<Value, ToolError> {
    let (kind, id) = title_key(args)?;
    if id > MAX_SAFE_INTEGER {
        return Err(bad("id is out of range"));
    }
    let title = json!({ "type": if kind == "series" { "tv" } else { "movie" }, "id": id });
    match op {
        "watchlist_add" | "watchlist_remove" => Ok(json!({ "title": title })),
        "rate" => {
            let value = match args.get("value") {
                Some(Value::Null) => Value::Null,
                Some(Value::String(v)) if matches!(v.as_str(), "dislike" | "like" | "love") => json!(v),
                _ => return Err(bad("value is dislike, like or love, or null to clear the rating")),
            };
            Ok(json!({ "title": title, "value": value }))
        }
        _ => {
            let value = match args.get("value") {
                None | Some(Value::Null) => true,
                Some(Value::Bool(v)) => *v,
                Some(_) => return Err(bad("value is true (seen) or false (unseen)")),
            };
            let mut out = json!({ "title": title, "value": value });
            let number = |key: &str, max: u64| match args.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(v) => v
                    .as_u64()
                    .filter(|n| *n <= max)
                    .map(Some)
                    .ok_or_else(|| bad(format!("{key} is a whole number from 0 to {max}"))),
            };
            let season = number("season", MAX_SAFE_INTEGER)?;
            let episode = number("episode", MAX_EPISODE)?;
            if (season.is_some() || episode.is_some()) && kind != "series" {
                return Err(bad("season and episode are for series"));
            }
            if episode.is_some() && season.is_none() {
                return Err(bad("an episode needs its season"));
            }
            for (key, n) in [("season", season), ("episode", episode)] {
                if let Some(n) = n {
                    out[key] = json!(n);
                }
            }
            Ok(out)
        }
    }
}

/// The write tools, as `tools/list` names them.
pub fn list() -> Value {
    let title = json!({
        "type": { "type": "string", "enum": ["movie", "series"] },
        "id": { "type": "integer", "description": "The id a Den result gave" },
    });
    let schema = |extra: Value, required: &[&str]| {
        let mut properties = title.clone();
        for (key, value) in extra.as_object().into_iter().flatten() {
            properties[key] = value.clone();
        }
        let mut required = required.to_vec();
        required.extend(["type", "id"]);
        json!({ "type": "object", "properties": properties, "required": required })
    };
    // Every op is a set, so repeating a request leaves the library as it was.
    let annotations = |destructive: bool| {
        json!({ "readOnlyHint": false, "destructiveHint": destructive, "idempotentHint": true,
                "openWorldHint": false })
    };
    let queued =
        "Nothing happens at once: the change is queued and a TV or Den Web applies it when next open.";
    json!([
        {
            "name": "den_watchlist_add",
            "title": "Add to the watchlist",
            "description": format!("Add a film or series to the person's Den watchlist. {queued}"),
            "inputSchema": schema(json!({}), &[]),
            "annotations": annotations(false),
        },
        {
            "name": "den_watchlist_remove",
            "title": "Remove from the watchlist",
            "description": format!("Take a film or series off the person's Den watchlist. {queued}"),
            "inputSchema": schema(json!({}), &[]),
            "annotations": annotations(true),
        },
        {
            "name": "den_mark_seen",
            "title": "Mark seen or unseen",
            "description": format!(
                "Mark a film, series, season or episode seen (or unseen with value false). For a series give no \
                 season to mark all of it, a season for that season, a season and episode for one episode; 0 is \
                 specials. {queued}"),
            "inputSchema": schema(json!({
                "value": { "type": "boolean", "default": true, "description": "false marks it unseen" },
                "season": { "type": "integer", "minimum": 0, "description": "Series only" },
                "episode": { "type": "integer", "minimum": 0, "maximum": 99999,
                             "description": "Series only; needs season" },
            }), &[]),
            "annotations": annotations(false),
        },
        {
            "name": "den_rate",
            "title": "Rate a title",
            "description": format!(
                "Set the person's rating of a film or series: dislike, like or love, or null to clear it. {queued}"),
            "inputSchema": schema(json!({
                "value": { "type": ["string", "null"], "enum": ["dislike", "like", "love", null] },
            }), &["value"]),
            "annotations": annotations(false),
        },
    ])
}

/// What a write tool's `structuredContent` holds.
pub fn output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "queued": { "type": "boolean" },
            "id": { "type": "string" },
            "note": { "type": "string" },
        },
        "required": ["queued", "id", "note"],
    })
}
