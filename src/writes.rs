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
    b64url_decode, key_id, open_claim, request_id_with_rng, seal_request_with_rng, Queue, Request,
    KEM_PUBLIC_LEN, MAX_EPISODE, MAX_SAFE_INTEGER,
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

/// The write tools, as `tools/list` names them, and the op each sends. `tv_status` sends none: it asks whether the TV
/// is open (den-spec §16); nor does `play_status`, which asks how an earlier play went (play-results-v1 §8).
pub const TOOLS: [(&str, &str); 8] = [
    ("den_watchlist_add", "watchlist_add"),
    ("den_watchlist_remove", "watchlist_remove"),
    ("den_mark_seen", "seen"),
    ("den_rate", "rate"),
    ("den_play_on_tv", "play"),
    ("den_play_trailer", "play_trailer"),
    ("den_tv_status", "tv_status"),
    ("den_play_status", "play_status"),
];

/// How often a play request asks whether the TV has taken it.
const TV_POLL: Duration = Duration::from_millis(400);
/// How long one held read of a play's results waits for the TV's next word: inside `EDGE_TIMEOUT`, and short enough
/// to look at the queue between reads.
const RESULT_HOLD_S: u64 = 2;
/// A TV that reports results and took a request without a word for this long refused it as it opened it.
const SILENT_TAKE: Duration = Duration::from_secs(4);

/// What a play says once a TV that does not report results has taken it: all den-edge can tell, not whether it played.
const TAKEN_NOTE: &str =
    "The TV picked it up, but it can't say whether playback started. If nothing starts playing, this connection may \
    not be allowed to play on the TV: connections made before TV playback need reconnecting with \"Allow changes to \
    my library and playing on my TV\" ticked.";
/// And when it has not yet.
const WAITING_NOTE: &str =
    "Sent to the TV, which has not picked it up yet. It may still be waking or opening Den. A play older than two \
    minutes is dropped, so ask again if the TV stays idle.";
/// How to ask again about a play that is not confirmed yet, without sending it twice.
const CHECK_LATER: &str =
    "Not confirmed yet: call den_play_status with this id to check again, rather than playing \
    it a second time.";
const TV_OFF: &str =
    "Open Den on the TV first — it isn't running, so it can't play anything yet. Nothing was sent.";
const CHOOSE_TV: &str = "Ask which one to play on, then call again with tv. Nothing was sent.";

/// `GET /assistant/tv`: whether a TV is awake, and each TV's own lane (den-spec §16 *TV lanes*), the one seen last
/// first. A den-edge from before lanes lists none.
struct Tvs {
    online: bool,
    last_seen: Option<u64>,
    lanes: Vec<Tv>,
}

struct Tv {
    lane: String,
    name: Option<String>,
    /// Registered with the household controller, which can wake the receiver and open Den. Older den-edge answers
    /// omit this, and are deliberately treated as false.
    wakeable: bool,
    online: bool,
    last_seen: Option<u64>,
    /// It says how each play went (play-results-v1 §6). Older den-edge answers omit this: false.
    results: bool,
}

impl Tv {
    fn called(&self) -> &str {
        self.name.as_deref().unwrap_or("a TV with no name")
    }

    fn can_receive(&self) -> bool {
        self.online || self.wakeable
    }
}

/// "A", "A and B", "A, B and C".
fn names<'a>(tvs: impl IntoIterator<Item = &'a Tv>) -> String {
    let names: Vec<&str> = tvs.into_iter().map(Tv::called).collect();
    match names.split_last() {
        Some((last, [])) => (*last).to_owned(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => String::new(),
    }
}

/// The TV a play goes to: its lane and name, whether it reports play results, and whether it was asleep (registered to
/// wake, not draining) when the play was sent.
struct Chosen {
    lane: String,
    name: String,
    results: bool,
    waking: bool,
}

/// How a play stands when its wait ends: the TV's own word (play-results-v1 §3), else what the queue says.
enum Outcome {
    Said(crate::results::PlayResult),
    /// A TV that reports results took it and said nothing: it refused it as it opened it (§7).
    TakenSilently,
    Taken,
    Queued,
}

#[derive(Clone)]
struct Dropbox {
    library: String,
    public: Vec<u8>,
}

pub struct Writes {
    pub(crate) secret: [u8; 32],
    edge: String,
    client: Client<HttpConnector, Full<Bytes>>,
    dropboxes: Mutex<HashMap<String, (Dropbox, Instant)>>,
    /// The lists the read tools share between calls (`reads.rs`).
    pub(crate) lists: crate::reads::Shared,
    /// How long a play request waits to see whether the TV took it.
    tv_wait: Duration,
    /// How long it waits for the TV's word on how it went, from a TV that reports play results.
    result_wait: Duration,
}

/// Why a change was not queued: a short code for the log and the words the model is told.
pub(crate) struct Refused(pub &'static str, pub String);

pub(crate) fn refuse(code: &'static str, message: &str) -> Refused {
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
            lists: Default::default(),
            tv_wait: config.tv_wait,
            result_wait: config.result_wait,
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
        let outcome = if op == "tv_status" {
            self.tv_status(caller).await
        } else if op == "play_status" {
            self.play_status(caller, args).await
        } else {
            let wanted = match args.get("tv") {
                None | Some(Value::Null) => None,
                Some(Value::String(name)) if !name.trim().is_empty() => Some(name.trim().to_owned()),
                Some(_) => return Err(bad("tv is the name of a TV, as den_tv_status lists them")),
            };
            let args = build_args(op, args)?;
            self.queue(caller, op, args, wanted.as_deref()).await
        };
        let status = match &outcome {
            Ok(answer) => match answer.get("state").and_then(Value::as_str) {
                Some(state) => state,
                None if op == "tv_status" => "ok",
                None => "queued",
            },
            Err(Refused(code, _)) => code,
        };
        if log {
            eprintln!("write tool={tool} status={status}");
        }
        outcome.map_err(|Refused(_, message)| ToolError(message))
    }

    async fn queue(
        &self,
        caller: &Caller,
        op: &str,
        args: Value,
        wanted: Option<&str>,
    ) -> Result<Value, Refused> {
        let denied = || {
            refuse(
                "no_write_grant",
                "This connection can't change your library — reconnect and allow changes.",
            )
        };
        let grant = caller.write.as_ref().ok_or_else(denied)?;
        let key = open_claim(&self.secret, &caller.session, &grant.claim).map_err(|_| denied())?;
        // A play that would go stale is not queued: its TV must be open, or registered with the household controller
        // that wakes it and opens Den. With several TVs it goes to the lane of the one meant; with none listed, to the
        // shared TV queue.
        let tv = Queue::of(op) == Queue::Tv;
        let target = if tv { self.target(&grant.token, wanted).await? } else { None };
        let lane = target.as_ref().map(|tv| tv.lane.clone());
        let dropbox = self.dropbox(&caller.session, &grant.token).await?;
        let mut rng = rng();
        let id = request_id_with_rng(&mut rng);
        let at = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let request = Request { library: &dropbox.library, grant: key.id(), id: &id, at, op, args: &args };
        let sealed = seal_request_with_rng(&dropbox.public, &key, &request, &mut rng)
            .map_err(|_| refuse("not_sealed", "Den could not prepare that change."))?;
        drop(key);
        let body = match (tv, &lane) {
            (true, Some(lane)) => json!({ "sealed": sealed, "queue": "tv", "tv": lane }),
            (true, None) => json!({ "sealed": sealed, "queue": "tv" }),
            (false, _) => json!({ "sealed": sealed }),
        };
        let (status, answer) =
            self.edge_call("POST", "/assistant/append", &grant.token, Some(body), MAX_ANSWER).await?;
        match (status.as_u16(), error_code(&answer)) {
            (200, _) if tv => {
                let seq = answer.get("seq").and_then(Value::as_u64);
                let outcome = match (&target, seq) {
                    (Some(chosen), Some(seq)) if chosen.results => {
                        self.outcome(&grant.token, &id, seq, &chosen.lane).await
                    }
                    (_, Some(seq)) if self.taken(&grant.token, seq, lane.as_deref()).await => Outcome::Taken,
                    _ => Outcome::Queued,
                };
                let name = target.as_ref().map(|tv| tv.name.as_str());
                let waking = target.as_ref().is_some_and(|tv| tv.waking);
                let mut answer = describe(&id, &outcome, name, waking);
                answer["queued"] = json!(true);
                if let Some(name) = name {
                    answer["tv"] = json!(name);
                }
                Ok(answer)
            }
            (200, _) => Ok(json!({ "queued": true, "id": id, "note": QUEUED_NOTE })),
            (401, _) => Err(refuse("unauthorized", "This connection has expired — reconnect Den and try again.")),
            (403, _) => Err(denied()),
            (404, Some("unknown_tv")) => Err(refuse(
                "tv_offline",
                "That TV is no longer registered to wake and hasn't had Den open lately. Nothing was sent.",
            )),
            (409, _) if tv => Err(refuse(
                "queue_full",
                "The TV already has several requests waiting; try again in a moment.",
            )),
            (429, _) if tv => Err(refuse(
                "rate_limited",
                "Too many play requests in a short while; wait a minute and try again.",
            )),
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

    /// Where a play goes: `None` the shared TV queue, when no TV has a lane (a TV from before lanes); otherwise the lane
    /// and name of the TV `wanted` names (any case), or of the sole TV that is online or wakeable. Refused, with the
    /// TVs named, when that TV cannot receive a play, no TV can, or several can and none was named.
    async fn target(&self, token: &str, wanted: Option<&str>) -> Result<Option<Chosen>, Refused> {
        let tvs = self.tv(token).await?;
        if tvs.lanes.is_empty() {
            return if tvs.online { Ok(None) } else { Err(refuse("tv_offline", TV_OFF)) };
        }
        let chosen = match wanted {
            Some(wanted) => {
                let Some(tv) = tvs.lanes.iter().find(|tv| tv.called().eq_ignore_ascii_case(wanted)) else {
                    return Err(refuse(
                        "unknown_tv",
                        &format!(
                            "There's no TV called {wanted}. The TVs are {}. Nothing was sent.",
                            names(&tvs.lanes)
                        ),
                    ));
                };
                if !tv.can_receive() {
                    return Err(refuse(
                        "tv_offline",
                        &format!(
                            "Open Den on {} first — it isn't running there or registered to wake. Nothing was sent.",
                            tv.called()
                        ),
                    ));
                }
                tv
            }
            None => {
                let possible: Vec<&Tv> = tvs.lanes.iter().filter(|tv| tv.can_receive()).collect();
                match possible.as_slice() {
                    // Awake, and on no lane: a TV from before lanes, draining the shared queue.
                    [] if tvs.online => return Ok(None),
                    [] => return Err(refuse("tv_offline", TV_OFF)),
                    [tv] => *tv,
                    several => {
                        let available = names(several.iter().copied());
                        return Err(refuse(
                            "choose_tv",
                            &format!("Den can play on {available}. {CHOOSE_TV}"),
                        ));
                    }
                }
            }
        };
        Ok(Some(Chosen {
            lane: chosen.lane.clone(),
            name: chosen.called().to_owned(),
            results: chosen.results,
            waking: !chosen.online,
        }))
    }

    /// Whether Den is open on a TV, when one was last seen (ms), and each TV's lane: `GET /assistant/tv`.
    async fn tv(&self, token: &str) -> Result<Tvs, Refused> {
        let (status, answer) = self.edge_call("GET", "/assistant/tv", token, None, MAX_ANSWER).await?;
        match status.as_u16() {
            200 => match answer.get("online").and_then(Value::as_bool) {
                Some(online) => Ok(Tvs {
                    online,
                    last_seen: answer.get("lastSeenAt").and_then(Value::as_u64),
                    lanes: answer
                        .get("tvs")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|tv| {
                            Some(Tv {
                                lane: tv.get("tv")?.as_str()?.to_owned(),
                                name: tv.get("name").and_then(Value::as_str).map(str::to_owned),
                                wakeable: tv.get("wakeable").and_then(Value::as_bool).unwrap_or(false),
                                online: tv.get("online")?.as_bool()?,
                                last_seen: tv.get("lastSeenAt").and_then(Value::as_u64),
                                results: tv.get("results").and_then(Value::as_bool).unwrap_or(false),
                            })
                        })
                        .collect(),
                }),
                None => Err(refuse(
                    "edge_error",
                    "Den could not reach the TV's status right now; try again later.",
                )),
            },
            401 => Err(refuse("unauthorized", "This connection has expired — reconnect Den and try again.")),
            403 => Err(refuse(
                "no_write_grant",
                "This connection can't play on the TV — reconnect and allow changes and playing on the TV.",
            )),
            other => {
                eprintln!("den-edge: tv answered {other} {}", error_code(&answer).unwrap_or("-"));
                Err(refuse("edge_error", "Den could not reach the TV's status right now; try again later."))
            }
        }
    }

    /// `den_tv_status`: whether Den is open on a TV now, and on which of the household's TVs.
    async fn tv_status(&self, caller: &Caller) -> Result<Value, Refused> {
        let grant = caller.write.as_ref().ok_or_else(|| {
            refuse("no_write_grant", "This connection can't play on the TV — reconnect and allow changes.")
        })?;
        let tvs = self.tv(&grant.token).await?;
        let awake: Vec<&Tv> = tvs.lanes.iter().filter(|tv| tv.online).collect();
        let possible: Vec<&Tv> = tvs.lanes.iter().filter(|tv| tv.can_receive()).collect();
        let wakeable = tvs.lanes.iter().any(|tv| tv.wakeable);
        let note = match (possible.as_slice(), awake.as_slice(), tvs.online) {
            ([], _, false) => {
                "No TV has Den open or is registered to wake; ask the person to open Den before \
                               playing anything."
                    .to_owned()
            }
            ([], _, true) => "Den is open on the TV.".to_owned(),
            ([tv], [], _) => format!("{} is registered to wake; a play goes there.", tv.called()),
            ([tv], [_], _) => format!("Den is open on {}; a play goes there.", tv.called()),
            (several, _, _) => {
                format!("Den can play on {}: name one with tv when playing.", names(several.iter().copied()))
            }
        };
        Ok(json!({
            "online": tvs.online,
            "wakeable": wakeable,
            "last_seen_at": tvs.last_seen.map(crate::reads::rfc3339),
            "tvs": tvs.lanes.iter().map(|tv| json!({
                "name": tv.called(),
                "online": tv.online,
                "wakeable": tv.wakeable,
                "last_seen_at": tv.last_seen.map(crate::reads::rfc3339),
            })).collect::<Vec<_>>(),
            "note": note,
        }))
    }

    /// How request `id` (queue entry `seq` on `lane`) went, for a TV that reports play results: its results read held
    /// (`GET /play/result/{address}?wait=`) until one is terminal or the wait ends, and between reads whether the TV
    /// took it — so a TV that took it and said nothing is told apart from one still waking.
    async fn outcome(&self, token: &str, id: &str, seq: u64, lane: &str) -> Outcome {
        let Some(address) = crate::results::address(id) else { return Outcome::Queued };
        let deadline = Instant::now() + self.result_wait;
        let mut count = 0;
        let mut best: Option<crate::results::PlayResult> = None;
        let mut taken_at: Option<Instant> = None;
        while Instant::now() < deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            let wait = left.as_secs().clamp(1, RESULT_HOLD_S);
            let path = format!("/play/result/{address}?after={count}&wait={wait}");
            let started = Instant::now();
            let sealed: Option<Vec<String>> =
                match self.edge_call("GET", &path, token, None, MAX_ANSWER).await {
                    Ok((status, answer)) if status.as_u16() == 200 => answer
                        .get("results")
                        .and_then(Value::as_array)
                        .map(|r| r.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect()),
                    // A den-edge from before results, or one that cannot answer: the queue is all there is.
                    _ => None,
                };
            let Some(sealed) = sealed else { break };
            if sealed.len() > count {
                count = sealed.len();
                best = crate::results::strongest(&crate::results::open(id, &sealed)).cloned();
                if best.as_ref().is_some_and(|r| r.state != crate::results::State::Preparing) {
                    break;
                }
            } else if started.elapsed() < Duration::from_millis(500) {
                // An early answer (den-edge's held reads per address are capped): never ask again at once.
                tokio::time::sleep(TV_POLL.min(left)).await;
            }
            if best.is_none() {
                if taken_at.is_none() && self.state(token, seq, Some(lane)).await == Some("taken") {
                    taken_at = Some(Instant::now());
                }
                if taken_at.is_some_and(|at| at.elapsed() >= SILENT_TAKE) {
                    return Outcome::TakenSilently;
                }
            }
        }
        match best {
            Some(result) => Outcome::Said(result),
            None if taken_at.is_some() || self.state(token, seq, Some(lane)).await == Some("taken") => {
                Outcome::Taken
            }
            None => Outcome::Queued,
        }
    }

    /// `GET /assistant/request/{seq}?queue=tv`: `queued` or `taken`, or `None` when den-edge could not say.
    async fn state(&self, token: &str, seq: u64, lane: Option<&str>) -> Option<&'static str> {
        let path = match lane {
            Some(lane) => format!("/assistant/request/{seq}?queue=tv&tv={lane}"),
            None => format!("/assistant/request/{seq}?queue=tv"),
        };
        match self.edge_call("GET", &path, token, None, MAX_ANSWER).await {
            Ok((status, answer)) if status.as_u16() == 200 => {
                match answer.get("state").and_then(Value::as_str) {
                    Some("taken") => Some("taken"),
                    Some("queued") => Some("queued"),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// `den_play_status`: how a play an earlier answer gave the id of stands now, from the TV's results alone.
    async fn play_status(&self, caller: &Caller, args: &Value) -> Result<Value, Refused> {
        let grant = caller.write.as_ref().ok_or_else(|| {
            refuse("no_write_grant", "This connection can't play on the TV — reconnect and allow changes.")
        })?;
        let id = args.get("id").and_then(Value::as_str).unwrap_or("");
        let Some(address) = crate::results::address(id) else {
            return Err(refuse("bad_id", "id is the id a den_play_on_tv or den_play_trailer answer gave."));
        };
        let (status, answer) =
            self.edge_call("GET", &format!("/play/result/{address}"), &grant.token, None, MAX_ANSWER).await?;
        if status.as_u16() != 200 {
            eprintln!("den-edge: play result answered {status} {}", error_code(&answer).unwrap_or("-"));
            return Err(refuse("edge_error", "Den could not reach the TV's word on that play right now."));
        }
        let sealed: Vec<String> = answer
            .get("results")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|s| s.as_str().map(str::to_owned))
            .collect();
        let outcome = match crate::results::strongest(&crate::results::open(id, &sealed)) {
            Some(result) => Outcome::Said(result.clone()),
            None => Outcome::Queued,
        };
        let mut answer = describe(id, &outcome, None, false);
        if matches!(outcome, Outcome::Queued) {
            answer["state"] = json!("unknown");
            answer["note"] = json!(
                "The TV has said nothing about that play: it may not have taken it, may not report how plays go, or \
                 the play is more than ten minutes old."
            );
        }
        Ok(answer)
    }

    /// Whether the TV took request `seq` within the wait, asking `GET /assistant/request/{seq}?queue=tv` (on its lane,
    /// when it has one) every moment. `false` is "not yet" as much as "never": it says nothing of whether anything
    /// played.
    async fn taken(&self, token: &str, seq: u64, lane: Option<&str>) -> bool {
        let deadline = Instant::now() + self.tv_wait;
        while Instant::now() < deadline {
            tokio::time::sleep(TV_POLL.min(self.tv_wait)).await;
            let path = match lane {
                Some(lane) => format!("/assistant/request/{seq}?queue=tv&tv={lane}"),
                None => format!("/assistant/request/{seq}?queue=tv"),
            };
            match self.edge_call("GET", &path, token, None, MAX_ANSWER).await {
                Ok((status, answer)) if status.as_u16() == 200 => {
                    if answer.get("state").and_then(Value::as_str) == Some("taken") {
                        return true;
                    }
                }
                _ => return false,
            }
        }
        false
    }

    /// The session's library and its drop-box public key, from den-edge, kept for a minute.
    async fn dropbox(&self, session: &str, token: &str) -> Result<Dropbox, Refused> {
        if let Some((dropbox, at)) = self.dropboxes.lock().unwrap_or_else(|e| e.into_inner()).get(session) {
            if at.elapsed() < DROPBOX_TTL {
                return Ok(dropbox.clone());
            }
        }
        let (status, answer) = self.edge_call("GET", "/assistant/dropbox", token, None, MAX_ANSWER).await?;
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

    /// One JSON exchange with den-edge as the caller, the access token as its bearer, its answer read as JSON.
    pub(crate) async fn edge_call(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<Value>,
        max_answer: usize,
    ) -> Result<(StatusCode, Value), Refused> {
        let (status, body) = self.edge_exchange(method, path, token, body, max_answer).await?;
        Ok((status, serde_json::from_slice(&body).unwrap_or(Value::Null)))
    }

    /// The same, with the answer's bytes as they came, for one the caller reads without building a `Value`.
    pub(crate) async fn edge_exchange(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: Option<Value>,
        max_answer: usize,
    ) -> Result<(StatusCode, Bytes), Refused> {
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
                Limited::new(body, max_answer).collect().await.map_err(|e| format!("unreadable: {e}"))?;
            Ok::<_, String>((parts.status, body.to_bytes()))
        };
        let (status, body) = tokio::time::timeout(EDGE_TIMEOUT, exchange)
            .await
            .map_err(|_| unreachable("no answer in time".into()))?
            .map_err(unreachable)?;
        Ok((status, body))
    }
}

/// A play's answer from how it stands: `state`, `confirmed` (only for the TV's own `playing`), `reason` for a failure,
/// and words that say which, naming the TV when it is known.
fn describe(id: &str, outcome: &Outcome, tv: Option<&str>, waking: bool) -> Value {
    use crate::results::State;
    let on = tv.unwrap_or("the TV");
    let (state, reason, note) = match outcome {
        Outcome::Said(result) => match (result.state, result.reason) {
            (State::Playing, _) => ("playing", None, format!("Playing on {on}.")),
            (State::Preparing, _) => {
                ("preparing", None, format!("{on} took it and is preparing a source. {CHECK_LATER}"))
            }
            (State::Failed, reason) => {
                let reason = reason.unwrap_or("failed");
                let note = match reason {
                    "not_allowed" => format!("{on} didn't play it: a content limit or the profile on the TV blocks it."),
                    "no_source" => format!(
                        "{on} found no playable source for it. Try another title, or open Den on the TV to pick a source."
                    ),
                    "no_trailer" => format!("{on} has no trailer for it."),
                    "expired" => format!(
                        "{on} got the request too late: a play is dropped after two minutes. Ask again to send a new one."
                    ),
                    "superseded" => format!("{on} is playing a newer request instead."),
                    _ => format!("{on} couldn't start it. Ask again to send a new one."),
                };
                ("failed", Some(reason), note)
            }
        },
        Outcome::TakenSilently => (
            "taken",
            None,
            format!(
                "{on} took it but didn't start it. This connection may not be allowed to play on the TV — reconnect \
                 with \"Allow changes to my library and playing on my TV\" ticked — or it arrived too late."
            ),
        ),
        Outcome::Taken => ("taken", None, TAKEN_NOTE.to_owned()),
        Outcome::Queued if waking => (
            "waking",
            None,
            format!("Waking {on} and opening Den; it hasn't taken the play yet. {CHECK_LATER}"),
        ),
        Outcome::Queued => ("queued", None, WAITING_NOTE.to_owned()),
    };
    let mut answer = json!({ "id": id, "state": state, "confirmed": state == "playing", "note": note });
    if let Some(reason) = reason {
        answer["reason"] = json!(reason);
    }
    answer
}

/// den-edge's `{"error": "<code>"}`.
pub(crate) fn error_code(answer: &Value) -> Option<&str> {
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

/// The platform's CSPRNG. If it fails there is no safe way on, so the call fails rather than sealing without it.
fn rng() -> rand_core::UnwrapErr<getrandom::SysRng> {
    rand_core::UnwrapErr(getrandom::SysRng)
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
        "watchlist_add" | "watchlist_remove" | "play_trailer" => Ok(json!({ "title": title })),
        "rate" => {
            let value = match args.get("value") {
                Some(Value::Null) => Value::Null,
                Some(Value::String(v)) if matches!(v.as_str(), "dislike" | "like" | "love") => json!(v),
                _ => return Err(bad("value is dislike, like or love, or null to clear the rating")),
            };
            Ok(json!({ "title": title, "value": value }))
        }
        // `seen` and `play` both take a series, a season or an episode; only `seen` says seen or unseen.
        _ => {
            let mut out = json!({ "title": title });
            if op == "seen" {
                out["value"] = match args.get("value") {
                    None | Some(Value::Null) => json!(true),
                    Some(Value::Bool(v)) => json!(v),
                    Some(_) => return Err(bad("value is true (seen) or false (unseen)")),
                };
            }
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
    // Playing twice plays twice, and it changes nothing in the library.
    let playing = json!({ "readOnlyHint": false, "destructiveHint": false, "idempotentHint": false,
                          "openWorldHint": false });
    let which_tv = json!({ "type": "string",
        "description": "Which TV, by the name den_tv_status lists; needed when more than one can receive a play" });
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
                "Set the person's rating of a film or series. Den's UI calls dislike \"Not for me\"; it is also a \
                 taste signal that steers recommendations away. Use dislike for \"not for me\", \"not interested\" \
                 or \"stop suggesting this\", like for \"I liked it\", love for \"I loved it\" or \"favourite\", and \
                 null to clear a rating. {queued}"),
            "inputSchema": schema(json!({
                "value": { "type": ["string", "null"], "enum": ["dislike", "like", "love", null],
                    "description": "dislike = Not for me (not interested, stop suggesting); like; love; null clears" },
            }), &["value"]),
            "annotations": annotations(false),
        },
        {
            "name": "den_play_on_tv",
            "title": "Play on the TV",
            "description": "Play a film, episode or series on the TV where the household watches, as its own Play \
                button would. For a series give no season to play it where it stands (the next episode or where \
                they left off), or a season and episode for one episode. The TV must have Den open or be registered \
                to wake and open it. When more than one TV can receive the play, the answer names them and asks for \
                tv. Waits briefly for the TV's word: state is playing (confirmed), failed with a reason, preparing or \
                waking (not confirmed yet; check with den_play_status), or only taken/queued for a TV that cannot \
                say. Never report playing unless confirmed is true.",
            "inputSchema": schema(json!({
                "season": { "type": "integer", "minimum": 0, "description": "Series only" },
                "episode": { "type": "integer", "minimum": 0, "maximum": 99999,
                             "description": "Series only; needs season" },
                "tv": which_tv,
            }), &[]),
            "annotations": playing,
        },
        {
            "name": "den_play_trailer",
            "title": "Play a trailer on the TV",
            "description": "Play a title's trailer on the TV, as the title page's trailer button would. The TV \
                must have Den open or be registered to wake and open it. With several TVs, as den_play_on_tv.",
            "inputSchema": schema(json!({ "tv": which_tv }), &[]),
            "annotations": playing,
        },
        {
            "name": "den_tv_status",
            "title": "Can Den play on the TV",
            "description": "Whether Den is open on the household's TV right now or the TV is registered to wake \
                and open it, and when Den was last seen; with several TVs, each by name. Check it before offering \
                to play something.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true, "idempotentHint": true, "openWorldHint": false },
        },
        {
            "name": "den_play_status",
            "title": "How a play on the TV went",
            "description": "How an earlier den_play_on_tv or den_play_trailer stands now, by the id its answer gave: \
                playing (confirmed), failed with a reason, or preparing. Use it instead of playing the same thing \
                again when an answer was not confirmed yet.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "string", "description": "The id the play's answer gave" } },
                "required": ["id"],
            },
            "annotations": { "readOnlyHint": true, "idempotentHint": true, "openWorldHint": false },
        },
    ])
}

/// What a write tool's `structuredContent` holds.
pub fn output_schema(tool: &str) -> Value {
    if tool == "den_tv_status" {
        return json!({
            "type": "object",
            "properties": {
                "online": { "type": "boolean" },
                "wakeable": { "type": "boolean" },
                "last_seen_at": { "type": ["string", "null"] },
                "tvs": { "type": "array", "items": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "online": { "type": "boolean" },
                        "wakeable": { "type": "boolean" },
                        "last_seen_at": { "type": ["string", "null"] },
                    },
                    "required": ["name", "online", "wakeable"],
                } },
                "note": { "type": "string" },
            },
            "required": ["online", "wakeable", "note"],
        });
    }
    let states = json!(["playing", "failed", "preparing", "waking", "taken", "queued"]);
    let reason = json!({ "type": "string",
        "enum": ["not_allowed", "no_source", "no_trailer", "expired", "superseded", "failed"],
        "description": "Why the TV did not play it, with state failed" });
    if tool == "den_play_status" {
        let mut states = states;
        states.as_array_mut().expect("a list").push(json!("unknown"));
        return json!({
            "type": "object",
            "properties": {
                "id": { "type": "string" },
                "state": { "type": "string", "enum": states },
                "confirmed": { "type": "boolean" },
                "reason": reason,
                "note": { "type": "string" },
            },
            "required": ["id", "state", "confirmed", "note"],
        });
    }
    json!({
        "type": "object",
        "properties": {
            "queued": { "type": "boolean" },
            "id": { "type": "string" },
            "state": { "type": "string", "enum": states },
            "confirmed": { "type": "boolean", "description": "True only when the TV said it is playing" },
            "reason": reason,
            "tv": { "type": "string", "description": "The TV it was sent to, when the household has several" },
            "note": { "type": "string" },
        },
        "required": ["queued", "id", "note"],
    })
}
