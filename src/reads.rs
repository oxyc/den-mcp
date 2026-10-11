//! Library reads (den-spec `wire/assistant-v1.md` §15): what an assistant may see of the watchlist, Continue Watching
//! and Seen history. Devices publish a projection of them, sealed under a read key only the household and this
//! server hold; den-edge stores the latest. A call opens the read key from the token's `dr` claim (sealed to
//! `MCP_WRITE_KEY`), fetches the projection with the caller's own token, opens it and names the titles through
//! den-atlas, exactly as the discovery tools do. The read key lives for that call.

use crate::tools::{bad, paging, string, title, title_url, Ctx, ToolError};
use crate::writes::{error_code, refuse, Refused, Writes};
use den_assistant::{open_claim, open_projection, open_read_claim, ReadKey};
use futures_util::future::join_all;
use serde_json::{json, Map, Value};
use std::collections::HashMap;

/// The read tools, as `tools/list` names them.
pub const TOOLS: [&str; 3] = ["den_watchlist", "den_continue_watching", "den_history"];

/// The largest projection answer read from den-edge.
const MAX_PROJECTION_ANSWER: usize = 8 * 1024 * 1024;

const DENIED: &str =
    "This connection can't read your lists — reconnect and allow reading your watchlist and history.";

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

    /// The projection the caller's connection may read, opened.
    async fn projection(&self, caller: &crate::auth::Caller) -> Result<Value, Refused> {
        let key = self.read_key(caller)?;
        let token = &caller.read.as_ref().expect("read_key checked it").token;
        let (status, answer) =
            self.edge_call("GET", "/assistant/projection", token, None, MAX_PROJECTION_ANSWER).await?;
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
                eprintln!("den-edge: projection answered {other} {}", error_code(&answer).unwrap_or("-"));
                return Err(refuse(
                    "edge_error",
                    "Den could not read your lists right now; try again later.",
                ));
            }
        }
        let field = |name: &str| answer.get(name).and_then(Value::as_str);
        let (Some(library), Some(grant), Some(sealed)) = (field("library"), field("grant"), field("sealed"))
        else {
            return Err(refuse("edge_error", "Den could not read your lists right now; try again later."));
        };
        // The projection is the connection's own: a grant other than the claim's is someone else's.
        if grant != key.grant() {
            return Err(refuse("grant_mismatch", DENIED));
        }
        open_projection(key.key(), library, grant, sealed)
            .map_err(|_| refuse("unreadable", "Den could not read your lists right now; try again later."))
    }
}

/// One read tool call. The log line is the tool and a status word, never what was asked or a key.
pub async fn call(ctx: &Ctx<'_>, writes: &Writes, tool: &str, args: &Value) -> Result<Value, ToolError> {
    let query = Query::parse(tool, args)?;
    let outcome = writes.projection(ctx.caller).await;
    if ctx.cfg.log_requests {
        eprintln!(
            "read tool={tool} status={}",
            outcome.as_ref().map_or_else(|Refused(code, _)| *code, |_| "ok")
        );
    }
    let projection = outcome.map_err(|Refused(_, message)| ToolError(message))?;
    Ok(answer(ctx, &projection, &query).await)
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

/// A projection entry whose title reads: its Den kind (`series` is the library's `tv`) and TMDB id.
fn title_of(entry: &Value) -> Option<(&'static str, u64)> {
    let title = entry.get("title")?;
    let kind = match title.get("type")?.as_str()? {
        "movie" => "movie",
        "tv" => "series",
        _ => return None,
    };
    Some((kind, title.get("id")?.as_u64().filter(|id| *id > 0)?))
}

/// The page asked for, named and dated, with how fresh it is and what it leaves out.
async fn answer(ctx: &Ctx<'_>, projection: &Value, query: &Query) -> Value {
    let entries: Vec<(&'static str, u64, &Value)> = projection
        .get(query.list)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| title_of(e).map(|(kind, id)| (kind, id, e)))
        .filter(|(kind, ..)| query.kind.is_none_or(|k| k == *kind))
        .collect();
    let at = |e: &Value| e.get("at").and_then(Value::as_u64);
    let timed = query.since.is_some() || query.until.is_some();
    let mut untimed = 0;
    let entries: Vec<_> = entries
        .into_iter()
        .filter(|(.., e)| {
            if !timed {
                return true;
            }
            match at(e) {
                Some(t) => query.since.is_none_or(|s| t >= s) && query.until.is_none_or(|u| t <= u),
                None => {
                    untimed += 1;
                    false
                }
            }
        })
        .collect();
    let total = entries.len();
    let shown: Vec<_> = entries.into_iter().skip(query.page * query.limit).take(query.limit).collect();

    let mut keys: Vec<(&'static str, u64)> = shown.iter().map(|(kind, id, _)| (*kind, *id)).collect();
    keys.sort_unstable();
    keys.dedup();
    let (cards, unnamed) = cards(ctx, &keys).await;

    let results: Vec<Value> = shown
        .iter()
        .map(|(kind, id, e)| {
            let mut item = match cards.get(&(*kind, *id)) {
                Some(Value::Object(card)) => card.clone(),
                _ => Map::new(),
            };
            let date = |key: &str| e.get(key).and_then(Value::as_u64).map(rfc3339);
            match query.list {
                "watchlist" => {
                    if let Some(added) = date("addedAt") {
                        item.insert("added".into(), json!(added));
                    }
                }
                "continue" => {
                    for key in ["action", "season", "episode", "fraction"] {
                        if let Some(v) = e.get(key) {
                            item.insert(key.into(), v.clone());
                        }
                    }
                    if let Some(at) = date("at") {
                        item.insert("last_watched".into(), json!(at));
                    }
                }
                _ => {
                    for key in ["season", "episode"] {
                        if let Some(v) = e.get(key) {
                            item.insert(key.into(), v.clone());
                        }
                    }
                    if let Some(at) = date("at") {
                        item.insert("watched".into(), json!(at));
                    }
                }
            }
            Value::Object(item)
        })
        .collect();

    let as_of = projection.get("at").and_then(Value::as_u64).map(rfc3339);
    let omitted =
        projection.pointer(&format!("/omitted/{}", query.list)).and_then(Value::as_u64).unwrap_or(0);
    let mut note = format!(
        "As of {}: the last time a TV or Den Web published your lists, so it lags anything done since, and \
         anything done while neither was open is not here.",
        as_of.as_deref().unwrap_or("an unknown time")
    );
    if omitted > 0 {
        note.push_str(&format!(" {omitted} more entries exist that the shared list leaves out."));
    }
    if untimed > 0 {
        note.push_str(&format!(" {untimed} entries with no date were left out of the time range."));
    }
    if unnamed > 0 {
        note.push_str(&format!(" {unnamed} titles could not be named just now."));
    }
    let shown_count = query.page * query.limit + results.len();
    json!({
        "results": results,
        "total": total,
        "page": query.page,
        "more": total > shown_count,
        "as_of": as_of,
        "omitted": omitted,
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
            "omitted": { "type": "integer" },
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
