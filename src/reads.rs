//! Library reads (den-spec `wire/assistant-v1.md` §15): what an assistant may see of the watchlist, Continue Watching
//! and Seen history. Devices publish a projection of them, sealed under a read key only the household and this
//! server hold; den-edge stores the latest. A call opens the read key from the token's `dr` claim (sealed to
//! `MCP_WRITE_KEY`), fetches the projection with the caller's own token, opens it and names the titles through
//! den-atlas, exactly as the discovery tools do. The read key lives for that call.
//!
//! The container this runs in has 32 MB, and a household's history is tens of thousands of rows, so nothing here
//! builds a `serde_json::Value` of it: the opened compact form is read once into packed rows (`projection.rs`), the
//! page asked for is picked out of those, and only that page is named through atlas. Calls from one session share
//! one open (the three tools are often called together) and the packed rows are kept for a short while, wiped when
//! they expire.

use crate::projection::{Continue, Lists, Rated, Reaction, Seen, Watch, NO_NUMBER, NO_TIME};
use crate::tools::{bad, paging, string, title, title_url, Ctx, ToolError};
use crate::writes::{error_code, refuse, Refused, Writes};
use den_assistant::{open_claim, open_projection_compact, open_read_claim, ReadKey};
use futures_util::future::join_all;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The read tools, as `tools/list` names them.
pub const TOOLS: [&str; 5] =
    ["den_watchlist", "den_continue_watching", "den_history", "den_ratings", "den_recommend"];

/// The largest projection answer read from den-edge: its parts come to at most 16 MiB of characters (§15).
const MAX_PROJECTION_ANSWER: usize = 17 * 1024 * 1024;

/// How long a session's opened lists are kept for its next call.
const KEPT_FOR: Duration = Duration::from_secs(30);
/// The most packed rows kept, across every session. A set past what is left is used by the calls in flight and then
/// dropped, never kept.
const KEPT_BYTES: usize = 4 * 1024 * 1024;
/// The most sessions tracked at once.
const SESSIONS: usize = 32;

const DENIED: &str =
    "This connection can't read your lists — reconnect and allow reading your watchlist and history.";
const UNREADABLE: &str = "Den could not read your lists right now; try again later.";

/// A session's opened lists, and when and for which grant they were.
struct Held {
    at: Instant,
    grant: String,
    lists: Arc<Lists>,
    bytes: usize,
}

type Slot = Arc<tokio::sync::Mutex<Option<Held>>>;

/// The lists the read tools share between calls: one slot a session, held across an open so concurrent calls wait
/// for it instead of each doing their own.
pub struct Shared {
    slots: Mutex<HashMap<String, Slot>>,
    kept: Arc<AtomicUsize>,
    /// Recommendation requests being built and asked at once: one.
    recommending: tokio::sync::Semaphore,
}

impl Default for Shared {
    fn default() -> Self {
        Shared { slots: Mutex::default(), kept: Arc::default(), recommending: tokio::sync::Semaphore::new(1) }
    }
}

impl Shared {
    fn slot(&self, session: &str) -> Slot {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = slots.get(session) {
            return slot.clone();
        }
        if slots.len() >= SESSIONS {
            // Empty slots nobody is waiting on go first; a slot someone holds keeps working if it is dropped here.
            slots.retain(|_, s| Arc::strong_count(s) > 1 || s.try_lock().map_or(true, |h| h.is_some()));
            if slots.len() >= SESSIONS {
                slots.clear();
            }
        }
        slots.entry(session.to_owned()).or_default().clone()
    }
}

/// Drop what a slot holds, and what it counted toward `kept`.
fn release(held: &mut Option<Held>, kept: &AtomicUsize) {
    if let Some(h) = held.take() {
        kept.fetch_sub(h.bytes, Ordering::Relaxed);
    }
}

impl Writes {
    /// The caller's read key, opened from `dr` for the token's session. When the token also carries a write grant,
    /// the two must be the same connection's.
    fn read_key(&self, caller: &crate::auth::Caller) -> Result<ReadKey, Refused> {
        let denied = || refuse("no_read_grant", DENIED);
        let access = caller.read.as_ref().ok_or_else(denied)?;
        let key = open_read_claim(&self.secret, &caller.session, &access.claim).map_err(|_| denied())?;
        if let Some(grant) =
            caller.write.as_ref().and_then(|w| open_claim(&self.secret, &caller.session, &w.claim).ok())
        {
            if grant.id() != key.grant() {
                return Err(denied());
            }
        }
        Ok(key)
    }

    /// Whether the token's `dr` opens for its session: the read tools are listed for no other.
    pub fn can_read(&self, caller: &crate::auth::Caller) -> bool {
        self.read_key(caller).is_ok()
    }

    /// The lists the caller's connection may read: those this session opened a moment ago, or opened now. Calls that
    /// arrive while one is opening wait for it and share it.
    async fn lists(&self, caller: &crate::auth::Caller) -> Result<Arc<Lists>, Refused> {
        let key = self.read_key(caller)?;
        let grant = key.grant().to_owned();
        let started = Instant::now();
        let slot = self.lists.slot(&caller.session);
        let mut held = slot.lock().await;
        if let Some(h) = held.as_ref() {
            if h.grant == grant && (h.at >= started || h.at.elapsed() < KEPT_FOR) {
                return Ok(h.lists.clone());
            }
        }
        release(&mut held, &self.lists.kept);
        let lists = Arc::new(self.open_lists(caller, &key).await?);
        drop(key);
        let bytes = lists.bytes();
        // Kept for the calls that follow, and for those already waiting on this open.
        if self.lists.kept.load(Ordering::Relaxed) + bytes <= KEPT_BYTES {
            self.lists.kept.fetch_add(bytes, Ordering::Relaxed);
            let at = Instant::now();
            *held = Some(Held { at, grant, lists: lists.clone(), bytes });
            let (slot, kept) = (slot.clone(), self.lists.kept.clone());
            tokio::spawn(async move {
                tokio::time::sleep(KEPT_FOR).await;
                let mut held = slot.lock().await;
                if held.as_ref().is_some_and(|h| h.at == at) {
                    release(&mut held, &kept);
                }
            });
        }
        Ok(lists)
    }

    /// The projection from den-edge, opened and packed.
    async fn open_lists(&self, caller: &crate::auth::Caller, key: &ReadKey) -> Result<Lists, Refused> {
        let unreadable = || refuse("unreadable", UNREADABLE);
        let token = &caller.read.as_ref().expect("read_key checked it").token;
        let (status, body) =
            self.edge_exchange("GET", "/assistant/projection", token, None, MAX_PROJECTION_ANSWER).await?;
        match status.as_u16() {
            200 => {}
            404 => {
                return Err(refuse(
                    "no_projection",
                    "Den hasn't shared your lists yet — open Den on the TV or web once, then try again.",
                ))
            }
            401 => {
                return Err(refuse(
                    "unauthorized",
                    "This connection has expired — reconnect Den and try again.",
                ))
            }
            403 => return Err(refuse("no_read_grant", DENIED)),
            other => {
                // An error body is a few bytes; anything bigger is not one.
                let answer =
                    (body.len() < 4096).then(|| serde_json::from_slice::<Value>(&body).ok()).flatten();
                eprintln!(
                    "den-edge: projection answered {other} {}",
                    answer.as_ref().and_then(error_code).unwrap_or("-")
                );
                return Err(refuse("edge_error", UNREADABLE));
            }
        }
        /// The answer, its parts read once into the strings the opening takes and nothing else kept.
        #[derive(Deserialize)]
        struct Answer {
            library: String,
            grant: String,
            set: String,
            parts: Vec<String>,
        }
        let answer: Answer = serde_json::from_slice(&body).map_err(|_| refuse("edge_error", UNREADABLE))?;
        // The answer's bytes are not needed again: free them before the parts are opened.
        drop(body);
        // The projection is the connection's own: a grant other than the claim's is someone else's.
        if answer.grant != key.grant() {
            return Err(refuse("grant_mismatch", DENIED));
        }
        let compact =
            open_projection_compact(key.key(), &answer.library, &answer.grant, &answer.set, &answer.parts)
                .map_err(|_| unreadable())?;
        drop(answer);
        Lists::parse(&compact).ok_or_else(unreadable)
    }
}

/// One read tool call. The log line is the tool and a status word, never what was asked or a key.
pub async fn call(ctx: &Ctx<'_>, writes: &Writes, tool: &str, args: &Value) -> Result<Value, ToolError> {
    let query = Query::parse(tool, args)?;
    let outcome = writes.lists(ctx.caller).await;
    if ctx.cfg.log_requests {
        eprintln!(
            "read tool={tool} status={}",
            outcome.as_ref().map_or_else(|Refused(code, _)| *code, |_| "ok")
        );
    }
    let lists = outcome.map_err(|Refused(_, message)| ToolError(message))?;
    if query.list == "recommend" {
        return recommend(ctx, writes, &lists, &query).await;
    }
    Ok(answer(ctx, &lists, &query).await)
}

/// What a read tool was asked.
struct Query {
    list: &'static str,
    /// `den_recommend`: titles on the watchlist may be recommended.
    include_watchlist: bool,
    kind: Option<&'static str>,
    /// `den_ratings`: only this rating.
    reaction: Option<Reaction>,
    since: Option<u64>,
    until: Option<u64>,
    page: usize,
    limit: usize,
}

impl Query {
    fn parse(tool: &str, args: &Value) -> Result<Query, ToolError> {
        let list = match tool {
            "den_watchlist" => "watchlist",
            "den_continue_watching" => "continue",
            "den_recommend" => "recommend",
            "den_ratings" => "rated",
            _ => "seen",
        };
        let reaction = match string(args, "value")? {
            None | Some("all") => None,
            // Den's UI calls dislike "Not for me".
            Some("dislike" | "not_for_me") => Some(Reaction::Dislike),
            Some("like") => Some(Reaction::Like),
            Some("love") => Some(Reaction::Love),
            Some(_) => return Err(bad("value is dislike (Not for me), like, love or all")),
        };
        let include_watchlist = match args.get("include_watchlist") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err(bad("include_watchlist is true or false")),
        };
        let kind = match string(args, "type")? {
            None | Some("all") => None,
            Some("movie") => Some("movie"),
            Some("series") => Some("series"),
            Some(_) => return Err(bad("type is movie, series or all")),
        };
        let time = |key: &str| match string(args, key)? {
            None => Ok(None),
            Some(text) => parse_time(text).map(Some).ok_or_else(|| {
                bad(format!("{key} is a UTC date (2026-01-31) or time (2026-01-31T20:00:00Z)"))
            }),
        };
        let (since, until) = if list == "seen" { (time("since")?, time("until")?) } else { (None, None) };
        if since.zip(until).is_some_and(|(s, u)| s > u) {
            return Err(bad("since is after until"));
        }
        let (page, limit) = paging(args, if list == "recommend" { 10 } else { 20 })?;
        Ok(Query { list, include_watchlist, kind, reaction, since, until, page, limit })
    }
}

/// The rows of one list that the query keeps, counted, and the page of them asked for. One pass, nothing collected
/// but the page.
fn window<'a, T>(
    rows: &'a [T],
    lists: &Lists,
    query: &Query,
    title: impl Fn(&T) -> u32,
    at: impl Fn(&T) -> u64,
    keep: impl Fn(&T) -> bool,
    untimed: &mut usize,
) -> (usize, Vec<&'a T>) {
    let timed = query.since.is_some() || query.until.is_some();
    let from = query.page.saturating_mul(query.limit);
    let to = from.saturating_add(query.limit);
    let (mut total, mut page) = (0, Vec::new());
    for row in rows {
        if !keep(row) || query.kind.is_some_and(|k| k != lists.title(title(row)).0) {
            continue;
        }
        if timed {
            let t = at(row);
            if t == NO_TIME {
                *untimed += 1;
                continue;
            }
            if query.since.is_some_and(|s| t < s) || query.until.is_some_and(|u| t > u) {
                continue;
            }
        }
        if (from..to).contains(&total) {
            page.push(row);
        }
        total += 1;
    }
    (total, page)
}

/// A time as RFC 3339, when the row has one.
fn date(ms: u64) -> Option<String> {
    (ms != NO_TIME).then(|| rfc3339(ms))
}

fn put(extra: &mut Map<String, Value>, key: &str, number: u32) {
    if number != NO_NUMBER {
        extra.insert(key.into(), json!(number));
    }
}

/// A row of the page: its title (an index into the table) and the fields of its own.
type Row = (u32, Map<String, Value>);

/// The page asked for, named and dated, with how fresh it is.
async fn answer(ctx: &Ctx<'_>, lists: &Lists, query: &Query) -> Value {
    let mut untimed = 0;
    // Each row of the page as its title and the fields of its own.
    let (total, rows): (usize, Vec<Row>) = match query.list {
        "watchlist" => {
            let (total, page) =
                window(&lists.watchlist, lists, query, |e: &Watch| e.title, |e| e.at, |_| true, &mut untimed);
            let rows = page
                .iter()
                .map(|e| {
                    let mut extra = Map::new();
                    if let Some(added) = date(e.at) {
                        extra.insert("added".into(), json!(added));
                    }
                    (e.title, extra)
                })
                .collect();
            (total, rows)
        }
        "continue" => {
            let (total, page) = window(
                &lists.continuing,
                lists,
                query,
                |e: &Continue| e.title,
                |e| e.at,
                |_| true,
                &mut untimed,
            );
            let rows = page
                .iter()
                .map(|e| {
                    let mut extra = Map::new();
                    extra.insert("action".into(), json!(if e.resume { "resume" } else { "next" }));
                    put(&mut extra, "season", e.season);
                    put(&mut extra, "episode", e.episode);
                    extra.insert("fraction".into(), json!((f64::from(e.fraction) * 1000.0).round() / 1000.0));
                    if let Some(at) = date(e.at) {
                        extra.insert("last_watched".into(), json!(at));
                    }
                    (e.title, extra)
                })
                .collect();
            (total, rows)
        }
        "rated" => {
            let (total, page) = window(
                &lists.rated,
                lists,
                query,
                |e: &Rated| e.title,
                |e| e.at,
                |e| query.reaction.is_none_or(|r| r == e.reaction),
                &mut untimed,
            );
            let rows = page
                .iter()
                .map(|e| {
                    let mut extra = Map::new();
                    // Not `rating`: that key is TMDB's, and nothing of TMDB's reaches a model under it.
                    extra.insert("reaction".into(), json!(e.reaction.name()));
                    if e.reaction == Reaction::Dislike {
                        extra.insert("not_for_me".into(), json!(true));
                    }
                    if let Some(at) = date(e.at) {
                        extra.insert("rated_at".into(), json!(at));
                    }
                    (e.title, extra)
                })
                .collect();
            (total, rows)
        }
        _ => {
            let (total, page) =
                window(&lists.seen, lists, query, |e: &Seen| e.title, |e| e.at, |_| true, &mut untimed);
            let rows = page
                .iter()
                .map(|e| {
                    let mut extra = Map::new();
                    put(&mut extra, "season", e.season);
                    put(&mut extra, "episode", e.episode);
                    if let Some(at) = date(e.at) {
                        extra.insert("watched".into(), json!(at));
                    }
                    (e.title, extra)
                })
                .collect();
            (total, rows)
        }
    };

    let mut keys: Vec<(&'static str, u64)> = rows.iter().map(|(title, _)| lists.title(*title)).collect();
    keys.sort_unstable();
    keys.dedup();
    let (cards, unnamed) = cards(ctx, &keys).await;

    let results: Vec<Value> = rows
        .into_iter()
        .map(|(title, extra)| {
            let mut item = match cards.get(&lists.title(title)) {
                Some(Value::Object(card)) => card.clone(),
                _ => Map::new(),
            };
            item.extend(extra);
            Value::Object(item)
        })
        .collect();

    let shown_count = query.page.saturating_mul(query.limit) + results.len();
    let as_of = rfc3339(lists.at);
    let mut note = format!(
        "As of {as_of}: the last time a TV or Den Web published your lists, so it lags anything done since, and \
         anything done while neither was open is not here."
    );
    if total > shown_count {
        note.push_str(&format!(" {} more on the next page.", total - shown_count));
    }
    if untimed > 0 {
        note.push_str(&format!(" {untimed} entries with no date were left out of the time range."));
    }
    if unnamed > 0 {
        note.push_str(&format!(" {unnamed} titles could not be named just now."));
    }
    json!({
        "results": results,
        "total": total,
        "page": query.page,
        "more": total > shown_count,
        "as_of": as_of,
        "note": note,
    })
}

/// Each title as a discovery tool gives it (name, year, genre, Den Web link), from den-atlas, which caches. A title
/// atlas does not index is `indexed: false` with its link; one atlas failed on is left unnamed (and counted).
async fn cards(ctx: &Ctx<'_>, keys: &[(&'static str, u64)]) -> (HashMap<(&'static str, u64), Value>, usize) {
    let lookups = keys.iter().map(|&(kind, id)| async move {
        (kind, id, ctx.atlas.get(&format!("/index/title/{kind}/{id}.json"), ctx.rid).await)
    });
    let mut unnamed = 0;
    let mut out = HashMap::new();
    for (kind, id, found) in join_all(lookups).await {
        let plain = json!({ "type": kind, "id": id, "url": title_url(ctx.cfg, kind, id, None) });
        let card = match found {
            Ok((answer, _)) if answer.get("indexed") == Some(&json!(true)) => title(ctx.cfg, &answer),
            Ok(_) => Some(
                json!({ "type": kind, "id": id, "indexed": false, "url": title_url(ctx.cfg, kind, id, None) }),
            ),
            Err(failed) => {
                eprintln!("atlas: {}", failed.detail);
                unnamed += 1;
                None
            }
        };
        out.insert((kind, id), card.unwrap_or(plain));
    }
    (out, unnamed)
}

// ---- den_recommend

/// How far back the library speaks for taste (Den Web's own window): a household's years-old titles outweigh what it
/// watches now.
const TASTE_WINDOW_MS: u64 = 180 * 86_400_000;
/// The most library and owned titles atlas takes in one request (`recommend.rs` `MAX_LIBRARY`, `MAX_OWNED`), and the
/// request body it refuses past 512 KiB.
const MAX_LIBRARY: usize = 5000;
const MAX_OWNED: usize = 10_000;
const MAX_BODY: usize = 500 * 1024;
/// The most slides atlas answers with.
const MAX_SLIDES: usize = 300;

/// What a title says about taste, as Den Web weighs it (`homeLibraryView.ts` `weightOf`): watched or part-watched
/// counts full, watchlisted 0.6, a like adds 0.5, a love 1, and a dislike ("Not for me") counts −1.5 outright.
struct Taste {
    weight: f64,
    at: u64,
    /// Watched or part-watched, not only watchlisted.
    engaged: bool,
    /// Rated: the household has given its verdict, so atlas must not show it again.
    rated: bool,
    disliked: bool,
}

fn taste(lists: &Lists) -> HashMap<u32, Taste> {
    let when = |at: u64| if at == NO_TIME { 0 } else { at };
    let held = |weight| Taste { weight, at: 0, engaged: false, rated: false, disliked: false };
    let mut by: HashMap<u32, Taste> = HashMap::new();
    for e in &lists.watchlist {
        let t = by.entry(e.title).or_insert_with(|| held(0.6));
        t.at = t.at.max(when(e.at));
    }
    for (title, at) in
        lists.continuing.iter().map(|e| (e.title, e.at)).chain(lists.seen.iter().map(|e| (e.title, e.at)))
    {
        let t = by.entry(title).or_insert_with(|| held(1.0));
        t.weight = 1.0;
        t.engaged = true;
        t.at = t.at.max(when(at));
    }
    for e in &lists.rated {
        let t = by.entry(e.title).or_insert_with(|| held(0.0));
        t.rated = true;
        t.at = t.at.max(when(e.at));
        match e.reaction {
            Reaction::Dislike => {
                t.weight = -1.5;
                t.disliked = true;
            }
            Reaction::Like => t.weight += 0.5,
            Reaction::Love => t.weight += 1.0,
        }
    }
    by
}

/// `POST /recommend`'s body for these lists, as Den Web builds its own: the library's titles with their weights (the
/// recent ones, 180 days), `owned`, every title atlas must not show — all that is watched, in progress or rated, and
/// the watchlist unless it may be recommended — and `hide`, the household's own rules. At most `MAX_BODY` bytes, the
/// library giving way.
fn recommend_body(
    lists: &Lists,
    taste: &HashMap<u32, Taste>,
    surface: &str,
    include_watchlist: bool,
    now_ms: u64,
    limit: usize,
) -> String {
    use std::fmt::Write as _;
    let newest_first = |a: &(u32, u64), b: &(u32, u64)| b.1.cmp(&a.1).then(a.0.cmp(&b.0));
    let entry = |out: &mut String, title: u32, weight: Option<(f64, u64)>| {
        let (kind, id) = lists.title(title);
        let _ = write!(out, r#"{{"type":"{kind}","id":{id}"#);
        if let Some((weight, at)) = weight {
            let _ = write!(out, r#","weight":{weight},"at":{at}"#);
        }
        out.push('}');
    };
    let mut owned: Vec<(u32, u64)> = taste
        .iter()
        .filter(|(_, t)| t.engaged || t.rated || !include_watchlist)
        .map(|(&i, t)| (i, t.at))
        .collect();
    owned.sort_unstable_by(newest_first);
    owned.truncate(MAX_OWNED);
    let mut owned_json = String::from("[");
    for (n, (title, _)) in owned.iter().enumerate() {
        if n > 0 {
            owned_json.push(',');
        }
        entry(&mut owned_json, *title, None);
    }
    owned_json.push(']');

    let mut library: Vec<(u32, u64)> = taste.iter().map(|(&i, t)| (i, t.at)).collect();
    let since = now_ms.saturating_sub(TASTE_WINDOW_MS);
    if library.iter().any(|(_, at)| *at >= since) {
        library.retain(|(_, at)| *at >= since);
    }
    library.sort_unstable_by(newest_first);
    let hide = serde_json::to_string(&lists.hide).unwrap_or_else(|_| "{}".into());
    let head = format!(
        r#"{{"version":1,"surface":"{surface}","now":"{}","limit":{limit},"hide":{hide},"owned":{owned_json},"library":["#,
        rfc3339(now_ms)
    );
    let mut out = head;
    for (n, (title, at)) in library.iter().take(MAX_LIBRARY).enumerate() {
        let mut one = String::new();
        let t = &taste[title];
        entry(&mut one, *title, Some((t.weight, *at)));
        if out.len() + one.len() + 3 > MAX_BODY {
            break;
        }
        if n > 0 {
            out.push(',');
        }
        out.push_str(&one);
    }
    out.push_str("]}");
    out
}

/// Den's own words for why atlas ranked a title, from the scorer's reason code. `quality` and `buzz` are leaves out:
/// they weigh ratings and popularity, which are not for a model.
fn why_of(reason: &str) -> Option<&'static str> {
    Some(match reason {
        "similar" => "Similar to what you've watched",
        "profile" => "Fits your taste",
        "people" => "Cast or creators you like",
        "franchise" => "From a franchise you watch",
        "arrived" => "New on streaming",
        "recent" => "Recently released",
        "upcoming" => "Coming soon",
        "timely" => "New or coming soon",
        _ => return None,
    })
}

/// What to watch next, ranked by atlas against this household's lists, as Den Web's billboard is.
async fn recommend(ctx: &Ctx<'_>, writes: &Writes, lists: &Lists, query: &Query) -> Result<Value, ToolError> {
    #[derive(Deserialize)]
    struct Ranked {
        slides: Vec<Slide>,
    }
    #[derive(Deserialize)]
    struct Slide {
        #[serde(rename = "type")]
        kind: String,
        id: u64,
        why: Option<Why>,
    }
    #[derive(Deserialize)]
    struct Why {
        reason: Option<String>,
    }
    let as_of = rfc3339(lists.at);
    let (from, to) =
        (query.page.saturating_mul(query.limit), query.page.saturating_add(1).saturating_mul(query.limit));
    if from >= MAX_SLIDES {
        return Ok(json!({
            "results": [], "page": query.page, "more": false, "as_of": as_of,
            "note": format!("Den ranks at most {MAX_SLIDES} titles; this page is past them."),
        }));
    }
    let surface = match query.kind {
        Some("movie") => "movies",
        Some("series") => "series",
        _ => "home",
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    // One at a time: a request is built from every title the household holds, so several at once would hold several.
    let turn = writes.lists.recommending.acquire().await.expect("the semaphore is never closed");
    let (engaged, watchlisted, disliked, body) = {
        let taste = taste(lists);
        let engaged = taste.values().filter(|t| t.engaged).count();
        let disliked = taste.values().filter(|t| t.disliked).count();
        // One more than the page, so a next page is known to exist.
        let body =
            recommend_body(lists, &taste, surface, query.include_watchlist, now_ms, (to + 1).min(MAX_SLIDES));
        (engaged, taste.values().filter(|t| !t.engaged && t.weight == 0.6).count(), disliked, body)
    };
    let answer = ctx.atlas.post("/recommend", bytes::Bytes::from(body), ctx.rid).await?;
    let ranked: Ranked = serde_json::from_slice(&answer)
        .map_err(|_| ToolError("Den's index answered without a ranking.".into()))?;
    drop(answer);
    drop(turn);
    let slides: Vec<(&'static str, u64, Option<&'static str>)> = ranked
        .slides
        .iter()
        .filter_map(|s| {
            let kind = match s.kind.as_str() {
                "movie" => "movie",
                "series" => "series",
                _ => return None,
            };
            let why = s.why.as_ref().and_then(|w| w.reason.as_deref()).and_then(why_of);
            (s.id > 0).then_some((kind, s.id, why))
        })
        .collect();
    let more = slides.len() > to;
    let shown: Vec<_> = slides.iter().skip(from).take(query.limit).collect();

    let mut keys: Vec<(&'static str, u64)> = shown.iter().map(|(kind, id, _)| (*kind, *id)).collect();
    keys.sort_unstable();
    keys.dedup();
    let (cards, unnamed) = cards(ctx, &keys).await;
    // A title on the watchlist, when the watchlist may be recommended.
    let on_watchlist: std::collections::HashSet<(&'static str, u64)> = if query.include_watchlist {
        lists
            .watchlist
            .iter()
            .map(|e| lists.title(e.title))
            .filter(|k| keys.binary_search(k).is_ok())
            .collect()
    } else {
        Default::default()
    };
    let results: Vec<Value> = shown
        .iter()
        .map(|(kind, id, why)| {
            let mut item = match cards.get(&(*kind, *id)) {
                Some(Value::Object(card)) => card.clone(),
                _ => Map::new(),
            };
            if let Some(why) = why {
                item.insert("why".into(), json!(why));
            }
            if on_watchlist.contains(&(*kind, *id)) {
                item.insert("on_watchlist".into(), json!(true));
            }
            Value::Object(item)
        })
        .collect();

    let mut note = format!(
        "Ranked by Den against the {engaged} titles watched or in progress and {watchlisted} on the watchlist, shaped \
         by the household's likes, loves and {disliked} Not for me marks, as of {as_of} (the last time a TV or Den \
         Web published your lists). Titles already watched, rated{} are left out, and so is what the household \
         hides. At most {MAX_SLIDES} titles, best first.",
        if query.include_watchlist { "" } else { " or on the watchlist" },
    );
    if more {
        note.push_str(" More on the next page.");
    }
    if unnamed > 0 {
        note.push_str(&format!(" {unnamed} titles could not be named just now."));
    }
    Ok(json!({ "results": results, "page": query.page, "more": more, "as_of": as_of, "note": note }))
}

// ---- dates

/// Days since 1970-01-01 of a civil date (Howard Hinnant's days-from-civil).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date of days since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// A Unix time in milliseconds as `2026-10-10T12:34:56Z`.
pub(crate) fn rfc3339(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", t / 3600, t % 3600 / 60, t % 60)
}

/// `2026-01-31` or `2026-01-31T20:00:00Z` (UTC, optional fraction of a second) as Unix milliseconds.
fn parse_time(text: &str) -> Option<u64> {
    let (date, time) = match text.split_once('T') {
        Some((date, time)) => (date, Some(time.strip_suffix('Z')?)),
        None => (text, None),
    };
    let mut parts = date.split('-');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return None;
    }
    let (y, m, d): (i64, i64, i64) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    if !(1..=12).contains(&m)
        || !(1..=31).contains(&d)
        || civil_from_days(days_from_civil(y, m, d)) != (y, m, d)
    {
        return None;
    }
    let mut secs = 0;
    if let Some(time) = time {
        let time = time.split_once('.').map_or(time, |(whole, fraction)| {
            if fraction.bytes().all(|b| b.is_ascii_digit()) {
                whole
            } else {
                ""
            }
        });
        let mut parts = time.split(':');
        let (h, mi, s): (i64, i64, i64) =
            (parts.next()?.parse().ok()?, parts.next()?.parse().ok()?, parts.next()?.parse().ok()?);
        if parts.next().is_some() || h > 23 || mi > 59 || s > 59 {
            return None;
        }
        secs = h * 3600 + mi * 60 + s;
    }
    u64::try_from((days_from_civil(y, m, d) * 86_400 + secs) * 1000).ok()
}

// ---- tools/list

/// The read tools, as `tools/list` names them.
pub fn list() -> Value {
    let limit = json!({ "type": "integer", "minimum": 1, "maximum": 100, "default": 20 });
    let page = json!({ "type": "integer", "minimum": 0, "default": 0, "description": "0-based page" });
    let kind = json!({ "type": "string", "enum": ["movie", "series", "all"], "default": "all" });
    let annotations = json!({ "readOnlyHint": true, "idempotentHint": true, "openWorldHint": false });
    let fresh =
        "As of the last time a TV or Den Web published the lists, not live. Paged: `total` and `more` say \
                 whether there is another page.";
    json!([
        {
            "name": "den_watchlist",
            "title": "The person's watchlist",
            "description": format!("The titles on the person's Den watchlist, newest added first, with when each was \
                added. {fresh}"),
            "inputSchema": { "type": "object", "properties": { "type": kind, "page": page, "limit": limit } },
            "annotations": annotations,
        },
        {
            "name": "den_continue_watching",
            "title": "Continue watching",
            "description": format!("What the person can pick up: a film or episode part-watched (`resume`, with \
                `fraction` done) or the episode after their last (`next`), latest first. {fresh}"),
            "inputSchema": { "type": "object", "properties": { "page": page, "limit": limit } },
            "annotations": annotations,
        },
        {
            "name": "den_history",
            "title": "Watch history",
            "description": format!("What the person has marked seen: films and episodes, newest first, with when. \
                `since` and `until` are UTC dates (2026-01-31) or times (2026-01-31T20:00:00Z); entries with no \
                date are only listed without them. {fresh}"),
            "inputSchema": { "type": "object", "properties": {
                "since": { "type": "string" }, "until": { "type": "string" },
                "type": kind, "page": page, "limit": limit } },
            "annotations": annotations,
        },
        {
            "name": "den_ratings",
            "title": "Ratings",
            "description": format!("What the person has rated, newest first, with when: love, like or dislike. Den's \
                UI calls dislike \"Not for me\". Use it for \"what have I loved\" or \"what did I mark not for me\"; \
                `value` picks one rating. {fresh}"),
            "inputSchema": { "type": "object", "properties": {
                "value": { "type": "string", "enum": ["love", "like", "dislike", "all"], "default": "all",
                           "description": "dislike = Not for me" },
                "type": kind, "page": page, "limit": limit } },
            "annotations": annotations,
        },
        {
            "name": "den_recommend",
            "title": "What to watch next",
            "description": "What the household should watch next, ranked by Den against what it has watched, added \
                and rated (as Den Web's billboard is), best first: use it for \"what should we watch\". Leaves out \
                what is watched, in progress or rated (Not for me included), what the household hides, and the \
                watchlist unless include_watchlist is true. Paged, up to 300 titles; `why` is Den's own short \
                reason where it has one. As of the last time a TV or Den Web published the lists.",
            "inputSchema": { "type": "object", "properties": {
                "type": kind,
                "include_watchlist": { "type": "boolean", "default": false,
                    "description": "Also recommend titles already on the watchlist (marked on_watchlist)" },
                "page": page,
                "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": 10 } } },
            "annotations": annotations,
        },
    ])
}

/// What a read tool's `structuredContent` holds.
pub fn output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "results": { "type": "array", "items": { "type": "object", "required": ["type", "id", "url"] } },
            "total": { "type": "integer" },
            "page": { "type": "integer" },
            "more": { "type": "boolean" },
            "as_of": { "type": "string" },
            "note": { "type": "string" },
        },
        // `total` is every list's but den_recommend's, whose ranking atlas cuts at 300.
        "required": ["results", "page", "more", "note"],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_round_trip_and_bad_ones_are_refused() {
        assert_eq!(rfc3339(1_790_000_000_000), "2026-09-21T14:13:20Z");
        assert_eq!(parse_time("2026-09-21T14:13:20Z"), Some(1_790_000_000_000));
        assert_eq!(parse_time("2026-09-21T14:13:20.250Z"), Some(1_790_000_000_000));
        assert_eq!(parse_time("2026-09-21"), Some(1_789_948_800_000));
        assert_eq!(parse_time("2024-02-29").map(rfc3339).as_deref(), Some("2024-02-29T00:00:00Z"));
        for bad in
            ["2026-02-30", "2026-13-01", "26-01-01", "2026-01-01T25:00:00Z", "2026-01-01T10:00:00", "soon"]
        {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_huge_library_is_asked_within_atlas_s_limits() {
        // 20,000 titles, all recent: atlas takes 10,000 owned, 5,000 library and 512 KiB of body.
        let rows = |n: usize, per: &str| {
            (0..n).map(|i| per.replace("{}", &i.to_string())).collect::<Vec<_>>().join(",")
        };
        let compact = format!(
            r#"{{"at":1,"titles":[{}],"watchlist":[],"continue":[],"seen":[{}]}}"#,
            rows(20_000, r#"["movie",100{}]"#),
            rows(20_000, "[{},null,null,1789000000000]"),
        );
        let lists = Lists::parse(compact.as_bytes()).unwrap();
        let held = taste(&lists);
        let body = recommend_body(&lists, &held, "home", false, 1_790_000_000_000, 11);
        assert!(body.len() <= 512 * 1024, "{} bytes", body.len());
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["owned"].as_array().unwrap().len(), MAX_OWNED);
        let library = body["library"].as_array().unwrap().len();
        assert!(library > 0 && library <= MAX_LIBRARY, "{library}");
    }
}
