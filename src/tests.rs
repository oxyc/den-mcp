//! The server end to end against a stub atlas: the MCP protocol, every tool, the token gate, and the rule that no
//! field atlas does not describe as Den's own ever reaches a model.

use crate::auth::tests::{claims, key, token};
use crate::{handle, unix_now, AppState};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, StatusCode};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

/// What the stub atlas was asked, in order.
type Asked = Arc<Mutex<Vec<String>>>;

/// Canned atlas answers, by path prefix. Every one carries fields a model must never see — TMDB's poster path,
/// genre ids, ratings and ids — so the allowlist is exercised on each.
fn canned(path: &str) -> Option<Value> {
    let card = |kind: &str, id: u64, title: &str, year: u64| {
        json!({ "type": kind, "id": id, "title": title, "posterPath": "/p.jpg", "year": year,
                "genreIds": [80], "primaryGenre": "Crime", "imdbId": "tt1", "originalLanguage": "sv",
                "score": 0.9, "f": { "pop": 0.8 }, "voteAverage": 7.9, "overview": "TMDB text" })
    };
    let mut tmdb_named = card("movie", 42, "Named by TMDB", 2001);
    tmdb_named["titleFrom"] = json!("tmdb");
    Some(if path == "/index/schema.json" {
        // A kind atlas grew after this server shipped: refused because the schema names it TMDB's.
        json!({ "tmdb": { "filterKinds": ["rating", "character", "futuretmdb"] } })
    } else if path.starts_with("/index/query.json?q=heists%20without%20gore") {
        // "without gore" read as something to leave out, matched to nothing Den can leave out.
        json!({
            "parse": { "leftover": "heists", "excluded": { "phrases": ["gore"], "countries": [], "decades": [],
                       "mediaTypes": [], "genres": [], "labels": [], "plotFacets": [], "basedOnKind": [],
                       "people": [], "titles": 0 } },
            "hits": [card("movie", 949, "Heat", 1995)], "total": 480, "ignored": [], "unknownValues": [],
        })
    } else if path.starts_with("/index/query.json") {
        json!({
            "parse": { "mediaType": null, "country": "SE", "decade": 1990, "genres": [53], "labels": ["Slow Burn"],
                       "plotFacets": [], "leftover": "", "excluded": null, "lambda": 0.5 },
            "people": [{ "qid": "Q1", "id": 77, "name": "Ann Actor", "credits": 3 }],
            "hits": [card("movie", 949, "Heat", 1995), card("series", 1396, "Breaking Bad", 2008), tmdb_named],
            "total": 3, "semantics": {}, "coverage": {},
            "ignored": ["country=FI"], "unknownValues": [],
        })
    } else if path.starts_with("/index/filter/all/titles.json?sel=like:movie-949") {
        json!({ "titles": [card("movie", 1, "Insomnia", 1997), card("series", 2, "The Bridge", 2011)],
                "total": 2, "order": "like:movie-949", "coverage": {}, "ignored": [] })
    } else if path.starts_with("/index/filter/all/titles.json?sel=like:movie-550") {
        json!({ "titles": [card("series", 2, "The Bridge", 2011), card("movie", 3, "The Hunt", 2012)],
                "total": 2, "order": "like:movie-550", "coverage": {}, "ignored": [] })
    } else if path.starts_with("/index/filter/") && path.contains("/titles.json") {
        json!({ "titles": [card("movie", 5, "Let the Right One In", 2008)], "total": 31, "order": "x",
                "coverage": { "language": { "count": 27, "denominator": 100 },
                              "rating": { "count": 90, "denominator": 100 } },
                "ignored": [], "unknownValues": ["mood:tense"], "denominator": 47613 })
    } else if path.contains("/values/") && !path.contains("/people/values/") {
        json!({ "kind": "person", "mode": "and", "complete": true,
                "values": [{ "id": "Q25191", "name": "Christopher Nolan", "count": 12, "tmdbId": 525 }] })
    } else if path.contains("/people/counts.json") {
        json!({ "total": 5, "traits": { "gender": {
            "mode": "single", "complete": true,
            "values": { "Q6581097": 3, "Q6581072": 2 },
            "labels": { "Q6581097": "male", "Q6581072": "female" } } } })
    } else if path.contains("/people.json") {
        let born = if path.contains("born:1980") { 1984 } else { 1976 };
        let mut answer = json!({
            "people": [{ "id": format!("Q{born}"), "name": format!("Actor {born}"), "tmdbId": 99, "credits": 4,
                         "roles": ["cast"], "gender": ["Q6581097"], "citizenship": ["Q34"],
                         "born": { "precision": "day", "date": format!("{born}-05-01"), "year": born } }],
            "total": 1, "labels": { "Q6581097": "male", "Q34": "Sweden" },
            "traitCoverage": { "gender": { "count": 98, "denominator": 100 } }, "coverage": {}, "ignored": [],
        });
        if born == 1984 {
            answer["people"][0]["died"] = json!({ "precision": "year", "year": 2024 });
        } else {
            // What they are known for (den-atlas#80): titles from the corpus, planted with a TMDB field.
            answer["people"][0]["knownFor"] = json!([card("movie", 949, "Heat", 1995), { "type": "movie" }]);
        }
        // An atlas that orders by name says so; one that names no order ordered by credits.
        if path.contains("order=name") {
            answer["order"] = json!("name");
        }
        answer
    } else if path.contains("/people/values/citizenship.json?q=iceland") {
        // Atlas's own trait lookup (den-atlas#80), for one question only: any other is a 404, as on an atlas
        // without the route.
        json!({ "kind": "citizenship", "values": [{ "id": "Q189", "name": "Iceland", "count": 4 }],
                "complete": true, "denominator": 100 })
    } else if path.starts_with("/index/filter/") && path.contains("/counts.json") {
        json!({ "total": 40, "denominator": 47613, "kinds": {
            "genre": { "values": { "80": 20, "18": 30 }, "complete": true },
            "rating": { "values": { "7": 10 }, "complete": true },
            "mood": { "values": { "Tense/Edge-of-seat": 10, "Slow-burn": 5 }, "complete": true },
            "region": { "values": { "scandinavian": 5, "nordic": 6 },
                        "labels": { "scandinavian": "Scandinavian", "nordic": "Nordic" }, "complete": true },
            "language": { "values": { "sv": 3, "en": 30 }, "complete": true },
            "person": { "values": { "Q1": 3 }, "labels": { "Q1": "Ann Actor" }, "complete": false } } })
    } else if path == "/index/title/movie/949.json" {
        let mut title = card("movie", 949, "Heat", 1995);
        title["indexed"] = json!(true);
        title["labels"] =
            json!({ "primaryGenre": "Crime", "animated": false, "subgenres": ["Heist"], "moods": [] });
        title["plotFacets"] = json!({ "ending": "tragic" });
        title["countries"] = json!(["US"]);
        title["makers"] = json!([{ "id": "Q1", "name": "Michael Mann", "tmdbId": 638 }]);
        title["cast"] = json!([{ "id": "Q2", "name": "Al Pacino" }]);
        title["castTotal"] = json!(40);
        title
    } else if path == "/index/studios/movie/949.json" {
        json!({ "studios": [{ "id": "Q159846", "name": "Warner Bros." }, { "id": "not-a-qid", "name": "x" }] })
    } else if path.starts_with("/index/title/") {
        json!({ "type": "movie", "id": 1, "indexed": false })
    } else {
        return None;
    })
}

async fn stub_atlas() -> (String, Asked) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let asked: Asked = Arc::default();
    let seen = asked.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let seen = seen.clone();
            let service = hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
                let seen = seen.clone();
                async move {
                    let path = req.uri().path_and_query().unwrap().to_string();
                    // The schema is read once an hour for the TMDB kinds; the tests count the questions.
                    if path != "/index/schema.json" {
                        seen.lock().unwrap().push(path.clone());
                    }
                    let revalidating = req.headers().get("if-none-match").is_some_and(|v| v == "\"v1\"");
                    // One title is stale at once, so asking for it again revalidates.
                    let max_age = if path.contains("/series/1396") { 0 } else { 3600 };
                    let cache_control = format!("public, max-age={max_age}");
                    let resp = match canned(&path) {
                        _ if revalidating => hyper::Response::builder()
                            .status(304)
                            .header("cache-control", cache_control)
                            .body(Full::new(Bytes::new())),
                        Some(body) => hyper::Response::builder()
                            .header("content-type", "application/json")
                            .header("etag", "\"v1\"")
                            .header("cache-control", cache_control)
                            .body(Full::new(Bytes::from(body.to_string()))),
                        None => hyper::Response::builder()
                            .status(404)
                            .body(Full::new(Bytes::from(r#"{"error":"not_found"}"#))),
                    };
                    Ok::<_, std::convert::Infallible>(resp.unwrap())
                }
            });
            tokio::spawn(async move {
                let io = hyper_util::rt::TokioIo::new(stream);
                let _ = hyper::server::conn::http1::Builder::new().serve_connection(io, service).await;
            });
        }
    });
    (format!("http://{addr}"), asked)
}

fn config(atlas: String) -> crate::config::Config {
    crate::config::Config {
        port: 0,
        atlas: crate::config::Atlas { base: atlas, timeout: std::time::Duration::from_secs(5) },
        public_origin: "https://den.example".into(),
        web_origin: "https://den.example".into(),
        issuer: "https://den.example".into(),
        token_keys: vec![key().verifying_key()],
        rate_per_minute: 600,
        rate_burst: 100,
        metrics_token: Some("m".into()),
        log_requests: false,
        allowed_origins: Vec::new(),
    }
}

struct Server {
    state: Arc<AppState>,
    asked: Asked,
    bearer: String,
}

impl Server {
    async fn new() -> Server {
        let (atlas, asked) = stub_atlas().await;
        Server::with(config(atlas), asked)
    }

    fn with(cfg: crate::config::Config, asked: Asked) -> Server {
        let bearer = format!("Bearer {}", token(&key(), &claims(unix_now())));
        Server { state: Arc::new(AppState::new(cfg)), asked, bearer }
    }

    async fn send(
        &self,
        method: &str,
        path: &str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> (StatusCode, hyper::HeaderMap, String) {
        let mut req = Request::builder().method(method).uri(path);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp =
            handle(self.state.clone(), req.body(Full::new(Bytes::from(body.to_owned()))).unwrap()).await;
        let (parts, body) = resp.into_parts();
        let body = String::from_utf8(body.collect().await.unwrap().to_bytes().to_vec()).unwrap();
        (parts.status, parts.headers, body)
    }

    /// A JSON-RPC request with this server's token; the whole JSON-RPC answer.
    async fn rpc(&self, method: &str, params: Value) -> Value {
        let body = json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params }).to_string();
        let (status, _, body) = self.send("POST", "/mcp", &body, &[("authorization", &self.bearer)]).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        serde_json::from_str(&body).unwrap()
    }

    /// A tool's answer, as the JSON its text carries, or the error text.
    async fn tool(&self, name: &str, arguments: Value) -> Result<Value, String> {
        let answer = self.rpc("tools/call", json!({ "name": name, "arguments": arguments })).await;
        let result = &answer["result"];
        let text = result["content"][0]["text"].as_str().unwrap().to_owned();
        if result["isError"] == true {
            return Err(text);
        }
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_allowlisted(&value);
        // The same answer as structured content, holding to the tool's declared output schema.
        assert_eq!(result["structuredContent"], value, "{name}");
        conforms(&value, &crate::tools::output_schema(name), name);
        Ok(value)
    }
}

/// `value` holds to `schema` as far as the schemas here go: types, required keys, and the same for what they hold.
fn conforms(value: &Value, schema: &Value, at: &str) {
    let typed = match schema["type"].as_str() {
        Some("object") => value.is_object(),
        Some("array") => value.is_array(),
        Some("string") => value.is_string(),
        Some("integer") => value.is_i64() || value.is_u64(),
        Some("boolean") => value.is_boolean(),
        _ => true,
    };
    assert!(typed, "{at}: {value} is not a {}", schema["type"]);
    for key in schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        assert!(value.get(key).is_some(), "{at}: {key} is required: {value}");
    }
    for (key, property) in schema["properties"].as_object().into_iter().flatten() {
        if let Some(v) = value.get(key) {
            conforms(v, property, &format!("{at}.{key}"));
        }
    }
    if let (Some(items), Some(list)) = (schema.get("items"), value.as_array()) {
        for v in list {
            conforms(v, items, &format!("{at}[]"));
        }
    }
}

/// No key atlas uses for TMDB's data, or for the numbers behind its ranking, appears anywhere in an answer.
fn assert_allowlisted(value: &Value) {
    const NEVER: &[&str] = &[
        "posterPath",
        "backdropPath",
        "overview",
        "genreIds",
        "imdbId",
        "tmdbId",
        "score",
        "f",
        "pop",
        "voteAverage",
        "votes",
        "rating",
        "popularity",
    ];
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                assert!(!NEVER.contains(&k.as_str()), "{k} reached an answer: {value}");
                assert_allowlisted(v);
            }
        }
        Value::Array(list) => list.iter().for_each(assert_allowlisted),
        Value::String(s) => assert!(s != "TMDB text" && s != "/p.jpg", "TMDB data reached an answer: {s}"),
        _ => {}
    }
}

#[tokio::test]
async fn initialize_and_list_the_tools() {
    let s = Server::new().await;
    let init = s
        .rpc(
            "initialize",
            json!({ "protocolVersion": "2025-06-18", "capabilities": {},
                                           "clientInfo": { "name": "test", "version": "1" } }),
        )
        .await;
    assert_eq!(init["id"], 7);
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "den-mcp");
    assert!(init["result"]["instructions"].as_str().unwrap().contains("url"));
    let (status, _, body) = s
        .send(
            "POST",
            "/mcp",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            &[("authorization", &s.bearer)],
        )
        .await;
    assert_eq!((status, body.as_str()), (StatusCode::ACCEPTED, ""), "a notification takes no answer");
    let tools = s.rpc("tools/list", json!({})).await;
    let names: Vec<&str> =
        tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "den_search",
            "den_filter_titles",
            "den_filter_values",
            "den_find_people",
            "den_title",
            "den_similar",
            "search",
            "fetch"
        ]
    );
    for tool in tools["result"]["tools"].as_array().unwrap() {
        assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
        assert_eq!(tool["outputSchema"]["type"], "object", "{tool}");
        assert_eq!(
            (&tool["annotations"]["readOnlyHint"], &tool["annotations"]["idempotentHint"]),
            (&json!(true), &json!(true))
        );
        assert!(tool["description"].as_str().unwrap().len() < 800, "a lean description: {}", tool["name"]);
    }
    assert_eq!(s.rpc("ping", json!({})).await["result"], json!({}));
}

#[tokio::test]
async fn calls_past_the_cap_are_told_to_come_back_and_atlas_sees_no_more_than_its_share() {
    let (atlas, asked) = stub_atlas().await;
    let mut cfg = config(atlas);
    cfg.atlas.timeout = std::time::Duration::from_millis(200);
    let s = Server::with(cfg, asked);
    // Every tool slot taken: the call is refused at once, as a result the model can act on, and atlas is not asked.
    let held = s.state.tool_slots.try_acquire_many(crate::MAX_TOOL_CALLS as u32).unwrap();
    let busy = s.tool("den_filter_titles", json!({ "sel": ["genre:80"] })).await.unwrap_err();
    assert!(busy.contains("busy"), "{busy}");
    assert!(s.asked.lock().unwrap().is_empty());
    drop(held);
    // Every request slot to atlas taken: a call waits within atlas's timeout and fails, never adding a request.
    let held = s.state.atlas.in_flight().try_acquire_many(crate::atlas::MAX_IN_FLIGHT as u32).unwrap();
    let waited = s.tool("den_filter_titles", json!({ "sel": ["genre:80"] })).await.unwrap_err();
    assert!(waited.contains("unavailable"), "{waited}");
    assert!(s.asked.lock().unwrap().is_empty());
    drop(held);
    assert!(s.tool("den_filter_titles", json!({ "sel": ["genre:80"] })).await.is_ok());
}

/// Calls that find the TMDB kinds stale together read atlas's schema once, not once each.
#[tokio::test]
async fn a_stale_schema_is_read_once_however_many_calls_find_it() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let reads = Arc::new(AtomicUsize::new(0));
    let seen = reads.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let seen = seen.clone();
            let service = hyper::service::service_fn(move |_req: Request<hyper::body::Incoming>| {
                let seen = seen.clone();
                async move {
                    seen.fetch_add(1, Ordering::SeqCst);
                    // Slow enough that every call finds the reading stale before the first read ends.
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    let body = json!({ "tmdb": { "filterKinds": ["rating", "character", "later"] } });
                    Ok::<_, std::convert::Infallible>(hyper::Response::new(Full::new(Bytes::from(
                        body.to_string(),
                    ))))
                }
            });
            tokio::spawn(async move {
                let io = hyper_util::rt::TokioIo::new(stream);
                let _ = hyper::server::conn::http1::Builder::new().serve_connection(io, service).await;
            });
        }
    });
    let atlas = crate::atlas::Atlas::new(format!("http://{addr}"), std::time::Duration::from_secs(5));
    let (a, b, c, d) = tokio::join!(
        atlas.tmdb_kinds(None),
        atlas.tmdb_kinds(None),
        atlas.tmdb_kinds(None),
        atlas.tmdb_kinds(None)
    );
    for kinds in [a, b, c, d] {
        assert!(kinds.contains(&"later".to_owned()), "{kinds:?}");
    }
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn protocol_errors_are_json_rpc_errors() {
    let s = Server::new().await;
    assert_eq!(s.rpc("resources/list", json!({})).await["error"]["code"], crate::mcp::METHOD_NOT_FOUND);
    let unknown = s.rpc("tools/call", json!({ "name": "den_nope", "arguments": {} })).await;
    assert_eq!(unknown["error"]["code"], crate::mcp::INVALID_PARAMS);
    let (_, _, body) = s.send("POST", "/mcp", "{nope", &[("authorization", &s.bearer)]).await;
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["error"]["code"], crate::mcp::PARSE_ERROR);
    let (status, headers, _) = s.send("GET", "/mcp", "", &[("authorization", &s.bearer)]).await;
    assert_eq!((status, &headers["allow"]), (StatusCode::METHOD_NOT_ALLOWED, &"POST".parse().unwrap()));
    let (status, _, _) = s
        .send("POST", "/mcp", "{}", &[("authorization", &s.bearer), ("mcp-protocol-version", "1999-01-01")])
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // A batch: answered member by member where the version has batches (2025-03-26, and a request naming none),
    // refused where it doesn't.
    let batch = r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","method":"notifications/x"},
                    {"jsonrpc":"2.0","id":2,"method":"tools/list"}]"#;
    for version in [None, Some("2025-03-26")] {
        let mut headers = vec![("authorization", s.bearer.as_str())];
        headers.extend(version.map(|v| ("mcp-protocol-version", v)));
        let (status, _, body) = s.send("POST", "/mcp", batch, &headers).await;
        let replies: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(replies.as_array().map(Vec::len), Some(2), "{version:?}: {body}");
        assert_eq!((&replies[0]["id"], &replies[1]["id"]), (&json!(1), &json!(2)));
    }
    let (_, _, body) = s
        .send("POST", "/mcp", batch, &[("authorization", &s.bearer), ("mcp-protocol-version", "2025-06-18")])
        .await;
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["error"]["code"], crate::mcp::INVALID_REQUEST);
    // A tool's own failure is a result the model reads, not a protocol error.
    let bad = s.tool("den_search", json!({ "query": "x" })).await;
    assert!(bad.unwrap_err().contains("at least 2"));
}

#[tokio::test]
async fn no_token_is_a_401_that_says_where_to_get_one() {
    let s = Server::new().await;
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    let (status, headers, _) = s.send("POST", "/mcp", body, &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        headers["www-authenticate"],
        "Bearer realm=\"den\", resource_metadata=\"https://den.example/.well-known/oauth-protected-resource/mcp\""
    );
    let (status, headers, _) = s.send("POST", "/mcp", body, &[("authorization", "Bearer a.b.c")]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(headers["www-authenticate"].to_str().unwrap().contains("error=\"invalid_token\""));
    let (status, _, metadata) = s.send("GET", "/.well-known/oauth-protected-resource/mcp", "", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&metadata).unwrap(),
        json!({ "resource": "https://den.example/mcp", "authorization_servers": ["https://den.example"],
                "scopes_supported": ["den:search"], "bearer_methods_supported": ["header"], "resource_name": "Den" })
    );
    // Another site's page cannot drive a browser-held token here.
    let (status, _, _) = s
        .send("POST", "/mcp", body, &[("authorization", &s.bearer), ("origin", "https://evil.example")])
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = s
        .send("POST", "/mcp", body, &[("authorization", &s.bearer), ("origin", "https://den.example")])
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_session_past_its_rate_is_told_when_to_come_back() {
    let (atlas, asked) = stub_atlas().await;
    let s = Server::with(crate::config::Config { rate_burst: 2, rate_per_minute: 1, ..config(atlas) }, asked);
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
    for _ in 0..2 {
        assert_eq!(s.send("POST", "/mcp", body, &[("authorization", &s.bearer)]).await.0, StatusCode::OK);
    }
    let (status, headers, _) = s.send("POST", "/mcp", body, &[("authorization", &s.bearer)]).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(headers.contains_key("retry-after"));
}

#[tokio::test]
async fn search_answers_titles_people_and_what_it_understood() {
    let s = Server::new().await;
    let answer = s
        .tool("den_search", json!({ "query": "slow-burn 90s thrillers", "language": "SV", "limit": 5 }))
        .await
        .unwrap();
    assert_eq!(
        s.asked.lock().unwrap()[0],
        "/index/query.json?q=slow-burn%2090s%20thrillers&language=sv&skip=0&limit=5"
    );
    assert_eq!(
        answer["results"][0],
        json!({ "type": "movie", "id": 949, "title": "Heat", "year": 1995, "genre": "Crime", "language": "sv",
                "url": "https://den.example/movie/949-heat" })
    );
    assert_eq!(answer["results"][1]["url"], "https://den.example/tv/1396-breaking-bad");
    assert_eq!(
        answer["understood"],
        json!({ "country": "SE", "decade": 1990, "labels": ["Slow Burn"], "genres": ["Thriller"] })
    );
    assert_eq!(
        answer["people"],
        json!([{ "id": "Q1", "name": "Ann Actor", "credits": 3, "url": "https://den.example/person/77-ann-actor" }])
    );
    assert!(answer["corpus"].as_str().unwrap().contains("not every title"));
    // A hit named only by TMDB is left out and counted; a parameter atlas did not apply is named.
    assert_eq!(answer["results"].as_array().unwrap().len(), 2);
    assert!(answer["left_out"].as_str().unwrap().starts_with("1 "));
    assert_eq!(answer["not_applied"], json!(["country=FI"]));
    // Search's count is how many titles the words reached, said as such and never as a total.
    assert_eq!(answer["candidates"], 3);
    assert!(answer.get("total").is_none());
    assert!(answer["candidates_note"].as_str().unwrap().contains("not a count"));
    assert!(answer.get("exclusion_note").is_none());
    // An exclusion read and matched to nothing says nothing was left out, and where to leave it out instead.
    let gore = s.tool("den_search", json!({ "query": "heists without gore" })).await.unwrap();
    assert!(gore["exclusion_note"].as_str().unwrap().contains("-warning:"), "{gore}");
    // A language by name, and a series' broadcaster by its Q-id.
    s.tool("den_search", json!({ "query": "noir", "language": "Swedish", "broadcaster": "q907311" }))
        .await
        .unwrap();
    assert!(s
        .asked
        .lock()
        .unwrap()
        .contains(&"/index/query.json?q=noir&language=sv&broadcaster=Q907311&skip=0&limit=10".to_owned()));
    assert!(s.tool("den_search", json!({ "query": "noir", "broadcaster": "Netflix" })).await.is_err());
}

#[tokio::test]
async fn filter_titles_builds_the_canonical_url_and_refuses_ratings() {
    let s = Server::new().await;
    let answer = s
        .tool("den_filter_titles", json!({ "type": "movie", "sel": ["language:SV", "genre:80", "decade:1995"], "page": 1, "limit": 10 }))
        .await
        .unwrap();
    assert_eq!(
        s.asked.lock().unwrap()[0],
        "/index/filter/movie/titles.json?sel=decade:1990,genre:80,language:sv&skip=10&limit=10"
    );
    assert_eq!((answer["total"].clone(), answer["more"].clone()), (json!(31), json!(true)));
    assert_eq!(answer["unknown_values"], json!(["mood:tense"]));
    assert_eq!(answer["out_of"], "47,613 indexed titles");
    // How much of the type each applied kind is on record for; a TMDB kind's share is never said.
    assert_eq!(answer["on_record"], json!({ "language": "27% of films" }));
    // Only popular order for now, and another is said rather than faked.
    let newest =
        s.tool("den_filter_titles", json!({ "sel": ["genre:80"], "order": "newest" })).await.unwrap();
    assert!(newest["order_note"].as_str().unwrap().contains("can't order titles by newest"));
    assert!(s.tool("den_filter_titles", json!({ "order": "best" })).await.is_err());
    // TMDB's kinds are refused: the two this server knows, and one only atlas's schema names.
    for sel in ["rating:8", "character:walter white", "futuretmdb:1"] {
        let refused = s.tool("den_filter_titles", json!({ "sel": [sel] })).await.unwrap_err();
        assert!(refused.contains("not offered"), "{sel}: {refused}");
    }
    // Den's own vocabulary, whatever the case or separators, said back; a value Den lacks refused with the nearest.
    let read = s
        .tool(
            "den_filter_titles",
            json!({ "sel": ["genre:action", "mood:tense edge of seat", "language:Swedish", "region:SCANDINAVIAN"] }),
        )
        .await
        .unwrap();
    assert_eq!(
        read["read_as"],
        json!([
            "genre:action as genre:28",
            "mood:tense edge of seat as mood:Tense/Edge-of-seat",
            "region:SCANDINAVIAN as region:scandinavian"
        ])
    );
    let asked = s.asked.lock().unwrap().clone();
    assert!(
        asked.contains(
            &"/index/filter/all/titles.json?sel=genre:28,language:sv,mood:Tense%2FEdge-of-seat,region:scandinavian&limit=20"
                .to_owned()
        ),
        "{asked:?}"
    );
    let unknown = s.tool("den_filter_titles", json!({ "sel": ["mood:tense"] })).await.unwrap_err();
    assert!(unknown.contains("Nearest: Tense/Edge-of-seat"), "{unknown}");
    assert!(s
        .tool("den_filter_titles", json!({ "sel": ["genre:nonsense"] }))
        .await
        .unwrap_err()
        .contains("Nearest"));
    // Refused as another tool's: a title like another, and a person trait.
    assert!(s
        .tool("den_filter_titles", json!({ "sel": ["like:movie-1"] }))
        .await
        .unwrap_err()
        .contains("den_similar"));
    assert!(s
        .tool("den_filter_titles", json!({ "sel": ["gender:Q1"] }))
        .await
        .unwrap_err()
        .contains("den_find_people"));
}

#[tokio::test]
async fn filter_values_resolve_names_title_kinds_and_person_traits() {
    let s = Server::new().await;
    let nolan = s.tool("den_filter_values", json!({ "kind": "person", "q": "Nolan" })).await.unwrap();
    // A person comes with their Den Web page.
    assert_eq!(
        nolan["values"],
        json!([{ "id": "Q25191", "name": "Christopher Nolan", "count": 12,
                 "url": "https://den.example/person/525-christopher-nolan" }])
    );
    assert!(s.asked.lock().unwrap().contains(&"/index/filter/all/values/person.json?q=nolan".to_owned()));
    // A short list comes whole from counts.json, named: "Swedish" finds sv whatever the kind calls it.
    let swedish = s.tool("den_filter_values", json!({ "kind": "language", "q": "Swedish" })).await.unwrap();
    assert_eq!(swedish["values"], json!([{ "id": "sv", "name": "Swedish", "count": 3 }]));
    let moods = s.tool("den_filter_values", json!({ "kind": "mood" })).await.unwrap();
    assert_eq!(moods["values"].as_array().unwrap().len(), 2);
    assert_eq!(moods["complete"], true);
    let gender = s.tool("den_filter_values", json!({ "kind": "gender", "q": "fem" })).await.unwrap();
    assert_eq!(gender["values"], json!([{ "id": "Q6581072", "name": "female", "people": 2 }]));
    // Atlas's trait lookup where it answers (den-atlas#80), its `complete` passed through.
    let iceland =
        s.tool("den_filter_values", json!({ "kind": "citizenship", "q": "Iceland" })).await.unwrap();
    assert_eq!(iceland["values"], json!([{ "id": "Q189", "name": "Iceland", "people": 4 }]));
    assert_eq!(iceland["complete"], true);
    // Where it doesn't (a 404), the commonest from people/counts.json, never called complete when atlas didn't say
    // so; a country named in q is its citizenship item from Wikidata's table all the same.
    let sweden = s.tool("den_filter_values", json!({ "kind": "citizenship", "q": "Sweden" })).await.unwrap();
    assert_eq!(sweden["values"], json!([{ "id": "Q34", "name": "Sweden" }]));
    assert_eq!(sweden["complete"], false);
    assert!(sweden["note"].as_str().unwrap().contains("commonest"));
    let overview = s.tool("den_filter_values", json!({ "sel": ["country:se"] })).await.unwrap();
    assert_eq!(overview["kinds"]["genre"][0], json!({ "id": "18", "name": "Drama", "count": 30 }));
    assert_eq!(overview["kinds"]["person"][0], json!({ "id": "Q1", "name": "Ann Actor", "count": 3 }));
    assert!(overview["kinds"].get("rating").is_none(), "ratings are not offered");
    assert!(s.tool("den_filter_values", json!({ "kind": "rating" })).await.is_err());
}

#[tokio::test]
async fn people_by_traits_and_an_age_range() {
    let s = Server::new().await;
    // Born 1976–1996 for 30–50 this year: the decades it spans, one question each, held to the exact years.
    let year = crate::year_of(unix_now());
    let answer = s
        .tool(
            "den_find_people",
            json!({ "sel": ["decade:2020"], "role": "cast", "gender": "male",
                                         "born_min": 1975, "born_max": 1985, "type": "movie" }),
        )
        .await
        .unwrap();
    let asked = s.asked.lock().unwrap().clone();
    assert_eq!(
        asked,
        [
            "/index/filter/movie/people.json?sel=decade:2020&traits=born:1970,gender:Q6581097,role:cast&limit=20",
            "/index/filter/movie/people.json?sel=decade:2020&traits=born:1980,gender:Q6581097,role:cast&limit=20",
        ]
    );
    let people = answer["results"].as_array().unwrap();
    assert_eq!(people.len(), 2);
    assert_eq!(
        people[0],
        json!({ "id": "Q1976", "name": "Actor 1976", "credits": 4, "roles": ["cast"], "gender": ["male"],
                "citizenship": ["Sweden"], "born": "1976-05-01", "age": year - 1976,
                "url": "https://den.example/person/99-actor-1976",
                "known_for": [{ "type": "movie", "id": 949, "title": "Heat", "year": 1995, "genre": "Crime",
                                "language": "sv", "url": "https://den.example/movie/949-heat" }] })
    );
    assert_eq!(answer["on_record"]["gender"], "98% of credited people");
    assert!(answer["notes"][0].as_str().unwrap().contains("not complete"));
    // Held to the years: 1976 is outside 1980–1985.
    let narrow = s.tool("den_find_people", json!({ "born_min": 1980, "born_max": 1985 })).await.unwrap();
    assert!(narrow["results"].as_array().unwrap().iter().all(|p| p["id"] != "Q1976"), "{narrow}");
    assert!(s.tool("den_find_people", json!({ "gender": "robot" })).await.is_err());
    // A citizenship by name or code is its Wikidata item, and the answer says how it was read.
    let swedes =
        s.tool("den_find_people", json!({ "citizenship": ["Swedish"], "type": "movie" })).await.unwrap();
    assert!(s.asked.lock().unwrap().last().unwrap().contains("traits=citizenship:Q34"));
    assert_eq!(swedes["read_as"], json!(["citizenship Swedish as Sweden (Q34)"]));
    let danes = s.tool("den_find_people", json!({ "citizenship": ["DK"], "type": "movie" })).await.unwrap();
    assert!(danes["read_as"][0].as_str().unwrap().contains("Q756617"));
    assert!(s.tool("den_find_people", json!({ "citizenship": ["Narnia"] })).await.is_err());
}

#[tokio::test]
async fn people_sort_as_asked_or_say_the_order_they_came_in() {
    let s = Server::new().await;
    let notes = |answer: &Value| {
        answer["notes"].as_array().unwrap().iter().map(|n| n.to_string()).collect::<String>()
    };
    let by_name = s.tool("den_find_people", json!({ "sel": ["decade:2020"], "sort": "name" })).await.unwrap();
    assert_eq!(
        s.asked.lock().unwrap()[0],
        "/index/filter/all/people.json?sel=decade:2020&order=name&limit=20"
    );
    assert!(notes(&by_name).contains("Sorted by name."), "{by_name}");
    // Prominence is atlas's default, so it is not spelled; an atlas that names no order ordered by credits.
    let default = s.tool("den_find_people", json!({ "sel": ["decade:2010"] })).await.unwrap();
    assert_eq!(s.asked.lock().unwrap()[1], "/index/filter/all/people.json?sel=decade:2010&limit=20");
    assert!(notes(&default).contains("Sorted by credits"), "{default}");
    let youngest =
        s.tool("den_find_people", json!({ "sel": ["decade:2000"], "sort": "youngest" })).await.unwrap();
    assert!(s.asked.lock().unwrap()[2].contains("order=born_desc"));
    assert!(notes(&youngest).contains("can't sort people by youngest"), "{youngest}");
    assert!(s.tool("den_find_people", json!({ "sort": "fame" })).await.is_err());
    // Living leaves out the actor born 1984, who has died, and says the page may be short.
    let living = s
        .tool("den_find_people", json!({ "born_min": 1980, "born_max": 1985, "living": true }))
        .await
        .unwrap();
    assert!(living["results"].as_array().unwrap().is_empty(), "{living}");
    assert!(notes(&living).contains("1 who have died were left out"), "{living}");
}

#[tokio::test]
async fn a_title_is_its_facts_and_an_unindexed_one_says_so() {
    let s = Server::new().await;
    let heat = s.tool("den_title", json!({ "type": "movie", "id": 949 })).await.unwrap();
    assert_eq!(heat["url"], "https://den.example/movie/949-heat");
    assert_eq!(heat["subgenres"], json!(["Heist"]));
    assert_eq!(heat["plot_facets"], json!({ "ending": "tragic" }));
    assert_eq!(
        heat["directors_writers"],
        json!([{ "id": "Q1", "name": "Michael Mann", "url": "https://den.example/person/638-michael-mann" }])
    );
    assert_eq!(heat["cast"], json!([{ "id": "Q2", "name": "Al Pacino" }]));
    assert!(heat["attribution"].as_str().unwrap().contains("CC BY-SA"));
    // Its studios from their own route, each a Q-id and a name; and the makers said to be one list.
    assert_eq!(heat["studios"], json!([{ "id": "Q159846", "name": "Warner Bros." }]));
    assert!(heat["makers_note"].as_str().unwrap().contains("one list"));
    let unknown = s.tool("den_title", json!({ "type": "movie", "id": 1 })).await.unwrap();
    assert_eq!(unknown["indexed"], false);
    assert!(s.tool("den_title", json!({ "type": "tv", "id": 1 })).await.is_err(), "series, not tv");
}

/// ChatGPT's research pair: `search` gives ids and urls, `fetch` a title's facts as text, both through the same
/// allowlist as the den_ tools.
#[tokio::test]
async fn search_and_fetch_answer_in_the_research_shape() {
    let s = Server::new().await;
    let found = s.tool("search", json!({ "query": "slow-burn 90s thrillers" })).await.unwrap();
    assert_eq!(
        found["results"][0],
        json!({ "id": "movie:949", "title": "Heat (1995)", "url": "https://den.example/movie/949-heat" })
    );
    assert_eq!(found["results"][1]["id"], "series:1396");
    let heat = s.tool("fetch", json!({ "id": "movie:949" })).await.unwrap();
    assert_eq!((&heat["id"], &heat["title"]), (&json!("movie:949"), &json!("Heat (1995)")));
    assert_eq!(heat["url"], "https://den.example/movie/949-heat");
    let text = heat["text"].as_str().unwrap();
    for line in [
        "Heat (1995), a film.",
        "Subgenres: Heist.",
        "Plot: ending tragic.",
        "Directors and writers: Michael Mann.",
    ] {
        assert!(text.contains(line), "{line}: {text}");
    }
    assert!(!text.contains("TMDB text"));
    assert!(heat["metadata"]["attribution"].as_str().unwrap().contains("CC BY-SA"));
    assert!(s.tool("fetch", json!({ "id": "movie:1" })).await.unwrap_err().contains("nothing on"));
    assert!(s.tool("fetch", json!({ "id": "949" })).await.is_err());
}

#[tokio::test]
async fn similar_to_one_title_narrowed_and_to_several_interleaved() {
    let s = Server::new().await;
    let heat = s
        .tool(
            "den_similar",
            json!({ "titles": [{ "type": "movie", "id": 949 }], "sel": ["region:Scandinavian"] }),
        )
        .await
        .unwrap();
    assert!(s.asked.lock().unwrap().contains(
        &"/index/filter/all/titles.json?sel=like:movie-949,region:scandinavian&limit=20".to_owned()
    ));
    assert_eq!(heat["results"][1]["url"], "https://den.example/tv/2-the-bridge");
    let both = s
        .tool(
            "den_similar",
            json!({ "titles": [{ "type": "movie", "id": 949 }, { "type": "movie", "id": 550 }] }),
        )
        .await
        .unwrap();
    let ids: Vec<u64> =
        both["results"].as_array().unwrap().iter().map(|t| t["id"].as_u64().unwrap()).collect();
    assert_eq!(ids, [1, 2, 3], "first of each, then second of each, each title once");
}

#[tokio::test]
async fn an_answer_is_kept_and_revalidated_by_etag() {
    let s = Server::new().await;
    let args = json!({ "type": "movie", "id": 949 });
    s.tool("den_title", args.clone()).await.unwrap();
    s.tool("den_title", args).await.unwrap();
    // The title and its studios, each asked once: the second answer came from memory.
    assert_eq!(s.asked.lock().unwrap().len(), 2, "the second answer came from memory");
    assert_eq!(s.state.atlas.used(), [2, 0, 2]);
    // Stale: asked again with its ETag, and atlas's 304 is the cached answer.
    let stale = json!({ "type": "series", "id": 1396 });
    s.tool("den_title", stale.clone()).await.unwrap();
    assert_eq!(s.tool("den_title", stale).await.unwrap()["indexed"], false);
    assert_eq!(s.state.atlas.used(), [2, 1, 3]);
    let (status, _, metrics) = s.send("GET", "/metrics", "", &[("authorization", "Bearer m")]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        metrics.contains("mcp_tool_calls_total{tool=\"den_title\",outcome=\"ok\",caller=\"guest\"} 4"),
        "{metrics}"
    );
    assert_eq!(s.send("GET", "/metrics", "", &[]).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn health_is_ok_with_keys_and_degraded_without() {
    let s = Server::new().await;
    assert_eq!(s.send("GET", "/health", "", &[]).await.2, r#"{"status":"ok"}"#);
    let (atlas, asked) = stub_atlas().await;
    let keyless = Server::with(crate::config::Config { token_keys: vec![], ..config(atlas) }, asked);
    assert!(keyless.send("GET", "/health", "", &[]).await.2.contains("no_token_keys"));
}

#[test]
fn the_year_is_the_civil_year() {
    assert_eq!(crate::year_of(0), 1970);
    assert_eq!(crate::year_of(1_790_000_000), 2026);
    assert_eq!(crate::year_of(951_782_400), 2000, "29 February 2000");
}
