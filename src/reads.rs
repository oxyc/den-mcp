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

use crate::projection::{Continue, Lists, Seen, Watch, NO_NUMBER, NO_TIME};
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
pub const TOOLS: [&str; 3] = ["den_watchlist", "den_continue_watching", "den_history"];

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
#[derive(Default)]
pub struct Shared {
    slots: Mutex<HashMap<String, Slot>>,
    kept: Arc<AtomicUsize>,
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
    Ok(answer(ctx, &lists, &query).await)
}

/// What a read tool was asked.
struct Query {
    list: &'static str,
    kind: Option<&'static str>,
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
            _ => "seen",
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
        let (page, limit) = paging(args, 20)?;
        Ok(Query { list, kind, since, until, page, limit })
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
    untimed: &mut usize,
) -> (usize, Vec<&'a T>) {
    let timed = query.since.is_some() || query.until.is_some();
    let from = query.page.saturating_mul(query.limit);
    let to = from.saturating_add(query.limit);
    let (mut total, mut page) = (0, Vec::new());
    for row in rows {
        if query.kind.is_some_and(|k| k != lists.title(title(row)).0) {
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
                window(&lists.watchlist, lists, query, |e: &Watch| e.title, |e| e.at, &mut untimed);
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
            let (total, page) =
                window(&lists.continuing, lists, query, |e: &Continue| e.title, |e| e.at, &mut untimed);
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
        _ => {
            let (total, page) = window(&lists.seen, lists, query, |e: &Seen| e.title, |e| e.at, &mut untimed);
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
fn rfc3339(ms: u64) -> String {
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
        "required": ["results", "total", "page", "more", "note"],
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
}
