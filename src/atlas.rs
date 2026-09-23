//! den-atlas over one pooled keep-alive client, with a small cache of its answers.
//!
//! Atlas marks every `/index/…` answer public for an hour with an ETag, and its answers change only with the
//! dataset, at most once a day. So a fresh copy is answered from memory, and a stale one is revalidated with
//! `If-None-Match`, where a 304 costs atlas nothing. Answers are kept parsed, so a hit costs no parse, and the cache
//! is bounded by the memory they take, so an idle server stays small.
//!
//! Every request to atlas — waiting for a slot, the head, the whole body — runs inside one deadline, so an atlas
//! that sends a byte and stalls cannot hold a call. Identical misses asked at once are one request. When atlas errs
//! and an answer it marked `stale-while-revalidate` is still within that window, the kept answer is given instead.

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{header, HeaderMap, Request, StatusCode};
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The largest answer read: a page of 100 cards is ~25 KB, counts.json ~60 KB, the schema ~64 KB. With
/// `MAX_IN_FLIGHT` requests at once this bounds what answers being read can take (8 MB) in a 32 MB container.
const MAX_ANSWER: usize = 512 * 1024;
/// Memory kept for answers (as parsed, `value_size`), and the longest any is kept fresh whatever atlas says.
const CACHE_BYTES: usize = 6 * 1024 * 1024;
const MAX_FRESH: Duration = Duration::from_secs(3600);
/// How often the TMDB kinds are re-read from atlas's schema, and how soon after a failed read.
pub const SCHEMA_EVERY: Duration = Duration::from_secs(3600);
pub const SCHEMA_RETRY: Duration = Duration::from_secs(60);

pub struct Atlas {
    base: String,
    timeout: Duration,
    client: Client<HttpConnector, Full<Bytes>>,
    cache: Mutex<Cache>,
    /// Answers used, by `Source`, for the metrics.
    used: [AtomicU64; 4],
    /// The filter kinds atlas's schema names as TMDB's, as last read, and the ETag they were read under.
    tmdb: Mutex<(Arc<[String]>, Option<String>)>,
    /// Requests to atlas in flight at once. A tool call can ask several (an age range, several seeds), so this caps
    /// what reaches atlas whatever the number of calls; a request waits for a slot within its own deadline.
    in_flight: tokio::sync::Semaphore,
    /// A lock per question being fetched, so the same miss asked at once is one request.
    misses: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

/// The most requests this server has at atlas at once.
pub const MAX_IN_FLIGHT: usize = 16;

/// The filter kinds whose values are TMDB's (den-atlas `tmdb.rs`): refused as filters and never listed. Atlas's
/// `/index/schema.json` names them in `tmdb.filterKinds`, and that list is read and added to these; these stand
/// alone when the schema has never been read, so an unreachable schema never lets one through.
pub const TMDB_KINDS: &[&str] = &["rating", "character"];

#[derive(Default)]
struct Cache {
    entries: HashMap<String, Entry>,
    bytes: usize,
}

struct Entry {
    value: Arc<Value>,
    /// The memory it takes, as `value_size` measures it, with its key.
    size: usize,
    etag: Option<String>,
    fresh_until: Instant,
    /// Until when it may stand in for an answer atlas failed to give (`stale-while-revalidate`).
    stale_until: Instant,
}

/// Why atlas gave no answer. `status` is atlas's own when it answered with an error.
#[derive(Debug)]
pub struct Failed {
    pub status: Option<u16>,
    pub detail: String,
}

/// Where an answer came from, for the metrics.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Source {
    Cache,
    Revalidated,
    Fetched,
    /// Kept past its freshness and given because atlas failed, within its `stale-while-revalidate`.
    Stale,
}

/// One exchange with atlas, read whole.
struct Exchanged {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Atlas {
    pub fn new(base: String, timeout: Duration) -> Self {
        let mut connector = HttpConnector::new();
        connector.set_nodelay(true);
        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(8)
            .build(connector);
        let floor: Arc<[String]> = TMDB_KINDS.iter().map(|k| (*k).to_owned()).collect();
        Atlas {
            base,
            timeout,
            client,
            cache: Mutex::default(),
            used: Default::default(),
            tmdb: Mutex::new((floor, None)),
            in_flight: tokio::sync::Semaphore::new(MAX_IN_FLIGHT),
            misses: Mutex::default(),
        }
    }

    /// `GET <path>` with the head and the whole body inside one deadline, a slot's wait included: an atlas that
    /// answers a head and then stalls fails the request at the deadline, not whenever it wakes.
    async fn exchange(&self, path: &str, rid: Option<&str>, etag: Option<&str>) -> Result<Exchanged, Failed> {
        let mut req = Request::get(format!("{}{path}", self.base)).header(header::ACCEPT, "application/json");
        if let Some(rid) = rid {
            req = req.header("x-request-id", rid);
        }
        if let Some(etag) = etag {
            req = req.header(header::IF_NONE_MATCH, etag);
        }
        let req = req.body(Full::new(Bytes::new())).map_err(|e| failed(None, e.to_string()))?;
        tokio::time::timeout(self.timeout, async {
            let _slot = self.in_flight.acquire().await.map_err(|_| failed(None, "shutting down".into()))?;
            let resp = self
                .client
                .request(req)
                .await
                .map_err(|e| failed(None, format!("atlas unreachable: {e}")))?;
            let (parts, body) = resp.into_parts();
            let body = Limited::new(body, MAX_ANSWER)
                .collect()
                .await
                .map_err(|e| failed(Some(parts.status.as_u16()), format!("atlas answer unreadable: {e}")))?
                .to_bytes();
            Ok(Exchanged { status: parts.status, headers: parts.headers, body })
        })
        .await
        .map_err(|_| failed(None, "atlas did not answer in time".into()))?
    }

    /// Slots free for requests to atlas; the tests hold them.
    #[cfg(test)]
    pub fn in_flight(&self) -> &tokio::sync::Semaphore {
        &self.in_flight
    }

    /// The filter kinds that are TMDB's: `TMDB_KINDS` and whatever atlas's schema added when it was last read. Never
    /// waits: the schema is read in the background (`refresh_tmdb_kinds`).
    pub fn tmdb_kinds(&self) -> Arc<[String]> {
        self.tmdb.lock().unwrap_or_else(|e| e.into_inner()).0.clone()
    }

    /// Read atlas's schema for the TMDB kinds, with the ETag of the last reading, and keep what it says; a failed read
    /// keeps the last good one. Whether the reading now stands on a read of atlas (a 200 or a 304).
    pub async fn refresh_tmdb_kinds(&self, rid: Option<&str>) -> bool {
        let etag = self.tmdb.lock().unwrap_or_else(|e| e.into_inner()).1.clone();
        let read = match self.exchange("/index/schema.json", rid, etag.as_deref()).await {
            Ok(read) => read,
            Err(e) => {
                eprintln!("atlas schema: {} — keeping the last TMDB kinds read", e.detail);
                return false;
            }
        };
        if read.status == StatusCode::NOT_MODIFIED && etag.is_some() {
            return true;
        }
        let listed = (read.status == StatusCode::OK)
            .then(|| parse(&read.body).ok())
            .flatten()
            .and_then(|schema| schema.pointer("/tmdb/filterKinds")?.as_array().cloned());
        let Some(listed) = listed else {
            eprintln!("atlas schema: HTTP {} without tmdb.filterKinds — keeping the last read", read.status);
            return false;
        };
        let mut kinds: Vec<String> = TMDB_KINDS.iter().map(|k| (*k).to_owned()).collect();
        for kind in listed.iter().filter_map(Value::as_str) {
            if !kinds.iter().any(|k| k == kind) {
                kinds.push(kind.to_owned());
            }
        }
        let etag = read.headers.get(header::ETAG).and_then(|v| v.to_str().ok()).map(str::to_owned);
        *self.tmdb.lock().unwrap_or_else(|e| e.into_inner()) = (kinds.into(), etag);
        true
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, Cache> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Answers used so far from the cache, revalidated, fetched, and stale on an atlas error.
    pub fn used(&self) -> [u64; 4] {
        self.used.each_ref().map(|n| n.load(Ordering::Relaxed))
    }

    /// `GET <path_and_query>` as JSON. `rid` goes along so atlas's log line joins this one.
    pub async fn get(&self, path: &str, rid: Option<&str>) -> Result<(Arc<Value>, Source), Failed> {
        let answer = self.fetch(path, rid).await;
        if let Ok((_, source)) = &answer {
            self.used[*source as usize].fetch_add(1, Ordering::Relaxed);
        }
        answer
    }

    /// The kept answer to `path` while it is fresh.
    fn fresh(&self, path: &str, now: Instant) -> Option<Arc<Value>> {
        let cache = self.cache();
        cache.entries.get(path).filter(|e| e.fresh_until > now).map(|e| e.value.clone())
    }

    async fn fetch(&self, path: &str, rid: Option<&str>) -> Result<(Arc<Value>, Source), Failed> {
        if let Some(value) = self.fresh(path, Instant::now()) {
            return Ok((value, Source::Cache));
        }
        // The same miss asked at once: the first asks atlas, the rest wait for it and take what it kept.
        let gate = {
            let mut misses = self.misses.lock().unwrap_or_else(|e| e.into_inner());
            misses.entry(path.to_owned()).or_default().clone()
        };
        let answer = {
            let _turn = gate.lock().await;
            match self.fresh(path, Instant::now()) {
                Some(value) => Ok((value, Source::Cache)),
                None => self.ask(path, rid).await,
            }
        };
        let mut misses = self.misses.lock().unwrap_or_else(|e| e.into_inner());
        if misses.get(path).is_some_and(|g| Arc::ptr_eq(g, &gate) && Arc::strong_count(g) <= 2) {
            misses.remove(path);
        }
        answer
    }

    async fn ask(&self, path: &str, rid: Option<&str>) -> Result<(Arc<Value>, Source), Failed> {
        let now = Instant::now();
        let etag = self.cache().entries.get(path).and_then(|e| e.etag.clone());
        let read = match self.exchange(path, rid, etag.as_deref()).await {
            Ok(read) if read.status.is_server_error() => Err(failed(
                Some(read.status.as_u16()),
                detail_of(&read.body)
                    .unwrap_or_else(|| format!("atlas answered HTTP {}", read.status.as_u16())),
            )),
            other => other,
        };
        let read = match read {
            Ok(read) => read,
            // Atlas failed: an answer it allowed to stand in while stale does, rather than no answer at all.
            Err(e) => {
                let cache = self.cache();
                return match cache.entries.get(path).filter(|entry| entry.stale_until > now) {
                    Some(entry) => {
                        eprintln!("atlas: {} — answering {path} from the kept copy", e.detail);
                        Ok((entry.value.clone(), Source::Stale))
                    }
                    None => Err(e),
                };
            }
        };
        let lifetime = lifetime(read.headers.get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()));
        if read.status == StatusCode::NOT_MODIFIED {
            let mut cache = self.cache();
            if let Some(entry) = cache.entries.get_mut(path) {
                let (fresh, stale) = lifetime.unwrap_or_default();
                entry.fresh_until = now + fresh.min(MAX_FRESH);
                entry.stale_until = entry.fresh_until + stale;
                return Ok((entry.value.clone(), Source::Revalidated));
            }
            return Err(failed(Some(304), "atlas answered 304 to a question not cached".into()));
        }
        if !read.status.is_success() {
            let detail = detail_of(&read.body)
                .unwrap_or_else(|| format!("atlas answered HTTP {}", read.status.as_u16()));
            return Err(failed(Some(read.status.as_u16()), detail));
        }
        let value = Arc::new(parse(&read.body)?);
        let etag = read.headers.get(header::ETAG).and_then(|v| v.to_str().ok()).map(str::to_owned);
        if let Some((fresh, stale)) = lifetime {
            let fresh_until = now + fresh.min(MAX_FRESH);
            self.keep(path, value.clone(), etag, fresh_until, fresh_until + stale);
        }
        Ok((value, Source::Fetched))
    }

    fn keep(
        &self,
        path: &str,
        value: Arc<Value>,
        etag: Option<String>,
        fresh_until: Instant,
        stale_until: Instant,
    ) {
        let size = value_size(&value) + path.len();
        if size > CACHE_BYTES / 8 {
            return;
        }
        let mut cache = self.cache();
        if let Some(old) = cache.entries.remove(path) {
            cache.bytes -= old.size;
        }
        // Evict what goes stale soonest until the new answer fits: cheap at a few hundred entries.
        while cache.bytes + size > CACHE_BYTES {
            let Some(victim) =
                cache.entries.iter().min_by_key(|(_, e)| e.stale_until).map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(old) = cache.entries.remove(&victim) {
                cache.bytes -= old.size;
            }
        }
        cache.bytes += size;
        cache.entries.insert(path.to_owned(), Entry { value, size, etag, fresh_until, stale_until });
    }

    /// Answers held, and the memory they take.
    pub fn cached(&self) -> (usize, usize) {
        let cache = self.cache();
        (cache.entries.len(), cache.bytes)
    }
}

fn failed(status: Option<u16>, detail: String) -> Failed {
    Failed { status, detail }
}

fn parse(body: &[u8]) -> Result<Value, Failed> {
    serde_json::from_slice(body).map_err(|e| failed(None, format!("atlas answer is not JSON: {e}")))
}

/// Atlas's own words for an error, where it gave any.
fn detail_of(body: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(body).ok()?;
    v.get("detail").or_else(|| v.get("error")).and_then(Value::as_str).map(str::to_owned)
}

/// The memory a parsed answer takes: each value's own size, and what its strings, lists and maps hold.
fn value_size(value: &Value) -> usize {
    std::mem::size_of::<Value>()
        + match value {
            Value::String(s) => s.capacity(),
            Value::Array(list) => list.iter().map(value_size).sum(),
            // A key, and the map's own entry overhead.
            Value::Object(map) => map
                .iter()
                .map(|(k, v)| k.capacity() + std::mem::size_of::<String>() + 16 + value_size(v))
                .sum(),
            _ => 0,
        }
}

/// How long an answer may be reused, and for how long after that it may stand in while atlas fails, from its
/// `Cache-Control`: `None` for one that must not be kept here (`private`, `no-store`, or no max-age).
fn lifetime(cache_control: Option<&str>) -> Option<(Duration, Duration)> {
    let cc = cache_control?;
    let directives: Vec<&str> = cc.split(',').map(str::trim).collect();
    if directives.iter().any(|d| d.eq_ignore_ascii_case("no-store") || d.eq_ignore_ascii_case("private")) {
        return None;
    }
    let seconds = |name: &str| {
        directives.iter().find_map(|d| d.strip_prefix(name)?.parse().ok()).map(Duration::from_secs)
    };
    Some((seconds("max-age=")?, seconds("stale-while-revalidate=").unwrap_or_default()))
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
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    #[test]
    fn only_a_shared_answer_with_a_max_age_is_kept() {
        assert_eq!(
            lifetime(Some("public, max-age=3600, stale-while-revalidate=86400")),
            Some((Duration::from_secs(3600), Duration::from_secs(86400)))
        );
        assert_eq!(lifetime(Some("public, max-age=60")), Some((Duration::from_secs(60), Duration::ZERO)));
        assert_eq!(lifetime(Some("private, max-age=60")), None);
        assert_eq!(lifetime(Some("no-store")), None);
        assert_eq!(lifetime(Some("public")), None);
        assert_eq!(lifetime(None), None);
    }

    #[test]
    fn encoding_is_encode_uri_component() {
        assert_eq!(encode("Psychological Thriller"), "Psychological%20Thriller");
        assert_eq!(encode("Campy/Cult"), "Campy%2FCult");
        assert_eq!(encode("Dark & Gritty"), "Dark%20%26%20Gritty");
        assert_eq!(encode("Amélie"), "Am%C3%A9lie");
        assert_eq!(encode("a-b_c.d!e~f*g'h(i)"), "a-b_c.d!e~f*g'h(i)");
    }

    #[test]
    fn a_kept_answer_is_sized_by_the_memory_it_takes() {
        let small = value_size(&serde_json::json!({ "a": 1 }));
        let big = value_size(&serde_json::json!({ "titles": vec!["x".repeat(100); 100] }));
        assert!(big > 100 * 100, "the strings count: {big}");
        assert!(big > small * 50);
    }

    /// A stub atlas whose every answer `respond` writes, raw, onto the socket: head and body as it likes, and as
    /// slowly. Counts the requests.
    async fn raw_atlas<F, Fut>(respond: F) -> (String, Arc<AtomicUsize>)
    where
        F: Fn(String) -> Fut + Send + Sync + Clone + 'static,
        Fut: std::future::Future<Output = Vec<(Vec<u8>, Duration)>> + Send,
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let asked = Arc::new(AtomicUsize::new(0));
        let seen = asked.clone();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (respond, seen) = (respond.clone(), seen.clone());
                tokio::spawn(async move {
                    loop {
                        let mut head = Vec::new();
                        let mut byte = [0u8; 1];
                        while !head.ends_with(b"\r\n\r\n") {
                            if stream.read(&mut byte).await.unwrap_or(0) == 0 {
                                return;
                            }
                            head.push(byte[0]);
                        }
                        seen.fetch_add(1, Ordering::SeqCst);
                        for (bytes, pause) in respond(String::from_utf8_lossy(&head).into_owned()).await {
                            tokio::time::sleep(pause).await;
                            if stream.write_all(&bytes).await.is_err() {
                                return;
                            }
                        }
                    }
                });
            }
        });
        (format!("http://{addr}"), asked)
    }

    fn answer(cache_control: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncache-control: {cache_control}\r\netag: \"v1\"\r\n\
             content-length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    /// A head, one byte of body, then nothing: the request fails at the deadline, not whenever atlas wakes.
    #[tokio::test]
    async fn an_atlas_that_stalls_mid_answer_fails_at_the_deadline() {
        let (base, _) = raw_atlas(|_| async {
            vec![
                (
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 100\r\n\r\n{"
                        .to_vec(),
                    Duration::ZERO,
                ),
                (b" ".to_vec(), Duration::from_secs(600)),
            ]
        })
        .await;
        let atlas = Atlas::new(base, Duration::from_millis(300));
        let started = Instant::now();
        let failed = atlas.get("/index/x.json", None).await.unwrap_err();
        assert!(failed.detail.contains("in time"), "{failed:?}");
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    }

    /// The same miss asked at once is one request; the rest take the answer it kept.
    #[tokio::test]
    async fn identical_misses_at_once_ask_atlas_once() {
        let (base, asked) = raw_atlas(|_| async {
            vec![(answer("public, max-age=3600", r#"{"ok":true}"#), Duration::from_millis(50))]
        })
        .await;
        let atlas = Atlas::new(base, Duration::from_secs(5));
        let path = "/index/same.json";
        let (a, b, c, d) = tokio::join!(
            atlas.get(path, None),
            atlas.get(path, None),
            atlas.get(path, None),
            atlas.get(path, None)
        );
        for answer in [a, b, c, d] {
            assert_eq!(answer.unwrap().0["ok"], true);
        }
        assert_eq!(asked.load(Ordering::SeqCst), 1);
        assert!(atlas.misses.lock().unwrap().is_empty(), "nothing is left behind");
    }

    /// Atlas failing: an answer it marked stale-while-revalidate stands in while within the window, and not after.
    #[tokio::test]
    async fn a_kept_answer_stands_in_while_atlas_fails_within_its_window() {
        let down = Arc::new(AtomicBool::new(false));
        let flag = down.clone();
        let (base, _) = raw_atlas(move |head| {
            let down = flag.load(Ordering::SeqCst);
            async move {
                if down {
                    return vec![(
                        b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n".to_vec(),
                        Duration::ZERO,
                    )];
                }
                let cc = if head.contains("/window") {
                    "public, max-age=0, stale-while-revalidate=3600"
                } else {
                    "public, max-age=0"
                };
                vec![(answer(cc, r#"{"n":1}"#), Duration::ZERO)]
            }
        })
        .await;
        let atlas = Atlas::new(base, Duration::from_secs(5));
        atlas.get("/window", None).await.unwrap();
        atlas.get("/nowindow", None).await.unwrap();
        down.store(true, Ordering::SeqCst);
        let (value, source) = atlas.get("/window", None).await.unwrap();
        assert_eq!((value["n"].as_i64(), source), (Some(1), Source::Stale));
        assert_eq!(
            atlas.get("/nowindow", None).await.unwrap_err().status,
            Some(503),
            "no window, no stand-in"
        );
        assert_eq!(atlas.used()[Source::Stale as usize], 1);
    }

    /// The TMDB kinds: read in the background, revalidated by ETag, and the last good reading kept when atlas fails.
    #[tokio::test]
    async fn the_tmdb_kinds_are_revalidated_and_kept_through_a_failure() {
        let down = Arc::new(AtomicBool::new(false));
        let flag = down.clone();
        let (base, asked) = raw_atlas(move |head| {
            let down = flag.load(Ordering::SeqCst);
            async move {
                if down {
                    return vec![(
                        b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\n\r\n".to_vec(),
                        Duration::ZERO,
                    )];
                }
                if head.to_ascii_lowercase().contains("if-none-match: \"v1\"") {
                    return vec![(
                        b"HTTP/1.1 304 Not Modified\r\netag: \"v1\"\r\ncontent-length: 0\r\n\r\n".to_vec(),
                        Duration::ZERO,
                    )];
                }
                vec![(
                    answer("public, max-age=3600", r#"{"tmdb":{"filterKinds":["rating","later"]}}"#),
                    Duration::ZERO,
                )]
            }
        })
        .await;
        let atlas = Atlas::new(base, Duration::from_secs(5));
        assert_eq!(&*atlas.tmdb_kinds(), ["rating", "character"], "the floor before any reading");
        assert_eq!(asked.load(Ordering::SeqCst), 0, "asking for them never reads the schema");
        assert!(atlas.refresh_tmdb_kinds(None).await);
        assert_eq!(&*atlas.tmdb_kinds(), ["rating", "character", "later"]);
        assert!(atlas.refresh_tmdb_kinds(None).await, "a 304 keeps the reading");
        assert_eq!(&*atlas.tmdb_kinds(), ["rating", "character", "later"]);
        down.store(true, Ordering::SeqCst);
        assert!(!atlas.refresh_tmdb_kinds(None).await);
        assert_eq!(&*atlas.tmdb_kinds(), ["rating", "character", "later"], "the last good reading stands");
    }
}
