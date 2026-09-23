//! den-atlas over one pooled keep-alive client, with a small cache of its answers.
//!
//! Atlas marks every `/index/…` answer public for an hour with an ETag, and its answers change only with the
//! dataset, at most once a day. So a fresh copy is answered from memory, and a stale one is revalidated with
//! `If-None-Match`, where a 304 costs atlas nothing. The cache is bounded in bytes so an idle server stays small.

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{header, Request, StatusCode};
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The largest answer read: a page of 100 cards is ~25 KB, counts.json ~60 KB.
const MAX_ANSWER: usize = 2 * 1024 * 1024;
/// Bytes of answers kept, and the longest any is kept fresh whatever atlas says.
const CACHE_BYTES: usize = 4 * 1024 * 1024;
const MAX_FRESH: Duration = Duration::from_secs(3600);

pub struct Atlas {
    base: String,
    timeout: Duration,
    client: Client<HttpConnector, Full<Bytes>>,
    cache: Mutex<Cache>,
    /// Answers used, by `Source`, for the metrics.
    used: [AtomicU64; 3],
    /// The filter kinds atlas's schema names as TMDB's, and until when that reading stands.
    tmdb_kinds: Mutex<Option<(Vec<String>, Instant)>>,
    /// Held while the schema is read, so calls that find the reading stale together read it once.
    tmdb_refresh: tokio::sync::Mutex<()>,
    /// Requests to atlas in flight at once. A tool call can ask several (an age range, several seeds), so this caps
    /// what reaches atlas whatever the number of calls; a request waits for a slot within its own timeout.
    in_flight: tokio::sync::Semaphore,
}

/// The most requests this server has at atlas at once.
pub const MAX_IN_FLIGHT: usize = 16;

/// The filter kinds whose values are TMDB's (den-atlas `tmdb.rs`): refused as filters and never listed. Atlas's
/// `/index/schema.json` names them in `tmdb.filterKinds`, and that list is read and added to these; these stand
/// alone when the schema cannot be read, so an unreachable schema never lets one through.
pub const TMDB_KINDS: &[&str] = &["rating", "character"];

#[derive(Default)]
struct Cache {
    entries: HashMap<String, Entry>,
    bytes: usize,
}

struct Entry {
    body: Bytes,
    etag: Option<String>,
    fresh_until: Instant,
}

/// Why atlas gave no answer. `status` is atlas's own when it answered with an error.
#[derive(Debug)]
pub struct Failed {
    pub status: Option<u16>,
    pub detail: String,
}

/// What a fetch cost, for `Server-Timing`-style accounting in the metrics.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Source {
    Cache,
    Revalidated,
    Fetched,
}

impl Atlas {
    pub fn new(base: String, timeout: Duration) -> Self {
        let mut connector = HttpConnector::new();
        connector.set_nodelay(true);
        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(8)
            .build(connector);
        Atlas {
            base,
            timeout,
            client,
            cache: Mutex::default(),
            used: Default::default(),
            tmdb_kinds: Mutex::default(),
            tmdb_refresh: tokio::sync::Mutex::new(()),
            in_flight: tokio::sync::Semaphore::new(MAX_IN_FLIGHT),
        }
    }

    /// Send `req`, once a slot is free, all within the request timeout.
    async fn send(
        &self,
        req: Request<Full<Bytes>>,
    ) -> Result<hyper::Response<hyper::body::Incoming>, Failed> {
        tokio::time::timeout(self.timeout, async {
            let _slot = self.in_flight.acquire().await.map_err(|_| failed(None, "shutting down".into()))?;
            self.client.request(req).await.map_err(|e| failed(None, format!("atlas unreachable: {e}")))
        })
        .await
        .map_err(|_| failed(None, "atlas did not answer in time".into()))?
    }

    /// Slots free for requests to atlas; the tests hold them.
    #[cfg(test)]
    pub fn in_flight(&self) -> &tokio::sync::Semaphore {
        &self.in_flight
    }

    /// The filter kinds that are TMDB's: `TMDB_KINDS` and whatever atlas's schema adds, read at most hourly (a
    /// minute after a failed read).
    pub async fn tmdb_kinds(&self, rid: Option<&str>) -> Vec<String> {
        let current = || {
            let now = Instant::now();
            let reading = self.tmdb_kinds.lock().unwrap_or_else(|e| e.into_inner());
            reading.as_ref().filter(|(_, until)| *until > now).map(|(kinds, _)| kinds.clone())
        };
        if let Some(kinds) = current() {
            return kinds;
        }
        // One read at a time; whoever waited finds the reading the first one made.
        let _reading = self.tmdb_refresh.lock().await;
        if let Some(kinds) = current() {
            return kinds;
        }
        let now = Instant::now();
        let mut kinds: Vec<String> = TMDB_KINDS.iter().map(|k| (*k).to_owned()).collect();
        let read = self.fetch_uncached("/index/schema.json", rid).await;
        let listed =
            read.as_ref().ok().and_then(|schema| schema.pointer("/tmdb/filterKinds")?.as_array().cloned());
        for kind in listed.iter().flatten().filter_map(|k| k.as_str()) {
            if !kinds.iter().any(|k| k == kind) {
                kinds.push(kind.to_owned());
            }
        }
        let stands = if listed.is_some() { MAX_FRESH } else { Duration::from_secs(60) };
        *self.tmdb_kinds.lock().unwrap_or_else(|e| e.into_inner()) = Some((kinds.clone(), now + stands));
        kinds
    }

    /// A GET whose answer is read once and not kept in the answer cache (the schema is large and read hourly).
    async fn fetch_uncached(&self, path: &str, rid: Option<&str>) -> Result<serde_json::Value, Failed> {
        let mut req = Request::get(format!("{}{path}", self.base)).header(header::ACCEPT, "application/json");
        if let Some(rid) = rid {
            req = req.header("x-request-id", rid);
        }
        let req = req.body(Full::new(Bytes::new())).map_err(|e| failed(None, e.to_string()))?;
        let resp = self.send(req).await?;
        let status = resp.status().as_u16();
        let body = Limited::new(resp.into_body(), MAX_ANSWER)
            .collect()
            .await
            .map_err(|e| failed(Some(status), format!("atlas answer unreadable: {e}")))?
            .to_bytes();
        if status != 200 {
            return Err(failed(Some(status), format!("atlas answered HTTP {status}")));
        }
        parse(&body)
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, Cache> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Answers used so far from the cache, revalidated, and fetched.
    pub fn used(&self) -> [u64; 3] {
        self.used.each_ref().map(|n| n.load(Ordering::Relaxed))
    }

    /// `GET <path_and_query>` as JSON. `rid` goes along so atlas's log line joins this one.
    pub async fn get(&self, path: &str, rid: Option<&str>) -> Result<(serde_json::Value, Source), Failed> {
        let answer = self.fetch(path, rid).await;
        if let Ok((_, source)) = &answer {
            self.used[*source as usize].fetch_add(1, Ordering::Relaxed);
        }
        answer
    }

    async fn fetch(&self, path: &str, rid: Option<&str>) -> Result<(serde_json::Value, Source), Failed> {
        let now = Instant::now();
        let etag = {
            let cache = self.cache();
            match cache.entries.get(path) {
                Some(e) if e.fresh_until > now => return parse(&e.body).map(|v| (v, Source::Cache)),
                Some(e) => e.etag.clone(),
                None => None,
            }
        };
        let mut req = Request::get(format!("{}{path}", self.base)).header(header::ACCEPT, "application/json");
        if let Some(rid) = rid {
            req = req.header("x-request-id", rid);
        }
        if let Some(etag) = &etag {
            req = req.header(header::IF_NONE_MATCH, etag.as_str());
        }
        let req = req.body(Full::new(Bytes::new())).map_err(|e| failed(None, e.to_string()))?;
        let resp = self.send(req).await?;
        let status = resp.status();
        let (parts, body) = resp.into_parts();
        let fresh_for = freshness(parts.headers.get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()));
        if status == StatusCode::NOT_MODIFIED {
            let mut cache = self.cache();
            if let Some(entry) = cache.entries.get_mut(path) {
                entry.fresh_until = now + fresh_for.unwrap_or_default();
                return parse(&entry.body).map(|v| (v, Source::Revalidated));
            }
            return Err(failed(Some(304), "atlas answered 304 to a question not cached".into()));
        }
        let body = Limited::new(body, MAX_ANSWER)
            .collect()
            .await
            .map_err(|e| failed(Some(status.as_u16()), format!("atlas answer unreadable: {e}")))?
            .to_bytes();
        if !status.is_success() {
            let detail = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| {
                    v.get("detail").or_else(|| v.get("error")).and_then(|d| d.as_str()).map(str::to_owned)
                })
                .unwrap_or_else(|| format!("atlas answered HTTP {}", status.as_u16()));
            return Err(failed(Some(status.as_u16()), detail));
        }
        let value = parse(&body)?;
        let etag = parts.headers.get(header::ETAG).and_then(|v| v.to_str().ok()).map(str::to_owned);
        if let Some(fresh_for) = fresh_for {
            self.keep(path, body, etag, now + fresh_for.min(MAX_FRESH));
        }
        Ok((value, Source::Fetched))
    }

    fn keep(&self, path: &str, body: Bytes, etag: Option<String>, fresh_until: Instant) {
        if body.len() > CACHE_BYTES / 8 {
            return;
        }
        let mut cache = self.cache();
        if let Some(old) = cache.entries.remove(path) {
            cache.bytes -= old.body.len() + path.len();
        }
        // Evict what goes stale soonest until the new answer fits: cheap at a few hundred entries.
        while cache.bytes + body.len() + path.len() > CACHE_BYTES {
            let Some(victim) =
                cache.entries.iter().min_by_key(|(_, e)| e.fresh_until).map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(old) = cache.entries.remove(&victim) {
                cache.bytes -= old.body.len() + victim.len();
            }
        }
        cache.bytes += body.len() + path.len();
        cache.entries.insert(path.to_owned(), Entry { body, etag, fresh_until });
    }

    /// Answers held, and their bytes.
    pub fn cached(&self) -> (usize, usize) {
        let cache = self.cache();
        (cache.entries.len(), cache.bytes)
    }
}

fn failed(status: Option<u16>, detail: String) -> Failed {
    Failed { status, detail }
}

fn parse(body: &[u8]) -> Result<serde_json::Value, Failed> {
    serde_json::from_slice(body).map_err(|e| failed(None, format!("atlas answer is not JSON: {e}")))
}

/// How long an answer may be reused, from its `Cache-Control`: `None` for one that must not be kept here
/// (`private`, `no-store`, or no max-age).
fn freshness(cache_control: Option<&str>) -> Option<Duration> {
    let cc = cache_control?;
    let directives: Vec<&str> = cc.split(',').map(str::trim).collect();
    if directives.iter().any(|d| d.eq_ignore_ascii_case("no-store") || d.eq_ignore_ascii_case("private")) {
        return None;
    }
    directives.iter().find_map(|d| d.strip_prefix("max-age=")?.parse().ok()).map(Duration::from_secs)
}

/// Percent-encoded as JavaScript's `encodeURIComponent` does — the spelling atlas's canonical URLs use.
pub fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_shared_answer_with_a_max_age_is_kept() {
        assert_eq!(
            freshness(Some("public, max-age=3600, stale-while-revalidate=86400")),
            Some(Duration::from_secs(3600))
        );
        assert_eq!(freshness(Some("private, max-age=60")), None);
        assert_eq!(freshness(Some("no-store")), None);
        assert_eq!(freshness(Some("public")), None);
        assert_eq!(freshness(None), None);
    }

    #[test]
    fn encoding_is_encode_uri_component() {
        assert_eq!(encode("Psychological Thriller"), "Psychological%20Thriller");
        assert_eq!(encode("Campy/Cult"), "Campy%2FCult");
        assert_eq!(encode("Dark & Gritty"), "Dark%20%26%20Gritty");
        assert_eq!(encode("Amélie"), "Am%C3%A9lie");
        assert_eq!(encode("a-b_c.d!e~f*g'h(i)"), "a-b_c.d!e~f*g'h(i)");
    }
}
