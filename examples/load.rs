//! The performance numbers in the README: startup, idle and loaded RSS, and what a tool call costs on top of atlas.
//!
//!   cargo build --release && cargo run --release --example load
//!
//! Starts a stub atlas that answers a 24-card page at once, and the release binary against it. Then, over one
//! keep-alive connection each: the same page asked of atlas directly, and as `den_filter_titles` through den-mcp —
//! uncached (atlas says `no-store`), so every call goes to atlas — and cached. The overhead is the difference.

#[path = "mint.rs"]
mod mint;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use std::time::{Duration, Instant};

const CALLS: usize = 5000;
const CONCURRENT: usize = 8;

fn page() -> String {
    let cards: Vec<serde_json::Value> = (0..24)
        .map(|i| {
            serde_json::json!({ "type": "movie", "id": 1000 + i, "title": format!("A Film Called {i}"),
                "posterPath": "/x.jpg", "year": 1990 + i, "genreIds": [80, 18], "primaryGenre": "Crime",
                "imdbId": "tt0000001", "originalLanguage": "sv" })
        })
        .collect();
    serde_json::json!({ "titles": cards, "total": 4000, "order": "abc", "coverage": {}, "ignored": [],
                        "denominator": 47613 })
    .to_string()
}

async fn stub_atlas() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let body = Bytes::from(page());
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let _ = stream.set_nodelay(true);
            let body = body.clone();
            let service = hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
                let body = body.clone();
                async move {
                    let path = req.uri().path_and_query().map(|p| p.to_string()).unwrap_or_default();
                    let cache = if path.contains("genre:18") { "public, max-age=3600" } else { "no-store" };
                    let resp = hyper::Response::builder()
                        .header("content-type", "application/json")
                        .header("cache-control", cache)
                        .body(Full::new(if path == "/index/schema.json" {
                            Bytes::from(r#"{"tmdb":{"filterKinds":["rating","character"]}}"#)
                        } else {
                            body
                        }))
                        .unwrap();
                    Ok::<_, std::convert::Infallible>(resp)
                }
            });
            tokio::spawn(async move {
                let io = hyper_util::rt::TokioIo::new(stream);
                let _ = hyper::server::conn::http1::Builder::new().serve_connection(io, service).await;
            });
        }
    });
    port
}

fn rss_kb(pid: u32) -> u64 {
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &pid.to_string()]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
}

fn percentile(sorted: &[Duration], p: f64) -> f64 {
    sorted[((sorted.len() as f64 - 1.0) * p).round() as usize].as_secs_f64() * 1000.0
}

type HttpClient = Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>>;

async fn timed(client: &HttpClient, req: impl Fn() -> Request<Full<Bytes>>, n: usize) -> Vec<Duration> {
    let mut times = Vec::with_capacity(n);
    for _ in 0..n {
        let started = Instant::now();
        let resp = client.request(req()).await.unwrap();
        assert!(resp.status().is_success(), "{}", resp.status());
        resp.into_body().collect().await.unwrap();
        times.push(started.elapsed());
    }
    times.sort();
    times
}

fn report(what: &str, times: &[Duration]) -> (f64, f64) {
    let (p50, p99) = (percentile(times, 0.5), percentile(times, 0.99));
    println!("{what:<44} p50 {p50:>7.3} ms   p99 {p99:>7.3} ms");
    (p50, p99)
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let atlas = stub_atlas().await;
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let origin = format!("http://127.0.0.1:{port}");
    let key = mint::dev_key();
    let started = Instant::now();
    let mut child =
        std::process::Command::new(env!("CARGO_MANIFEST_DIR").to_owned() + "/target/release/den-mcp")
            .env("PORT", port.to_string())
            .env("ATLAS_URL", format!("http://127.0.0.1:{atlas}"))
            .env("PUBLIC_ORIGIN", &origin)
            .env("TOKEN_PUBLIC_KEYS", mint::b64url(key.verifying_key().as_bytes()))
            .env("RATE_PER_MINUTE", "1000000")
            .env("RATE_BURST", "1000000")
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("build the release binary first: cargo build --release");
    let client: HttpClient = Client::builder(TokioExecutor::new()).build_http();
    let health = || Request::get(format!("{origin}/health")).body(Full::new(Bytes::new())).unwrap();
    loop {
        if client.request(health()).await.is_ok_and(|r| r.status().is_success()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    println!(
        "startup (spawn to first /health answer)        {:.1} ms",
        started.elapsed().as_secs_f64() * 1000.0
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    println!("RSS idle                                       {} KB", rss_kb(child.id()));

    let token = format!("Bearer {}", mint::token(&key, &origin, "load", 3600));
    let call = |sel: &'static str| {
        let token = token.clone();
        let origin = origin.clone();
        move || {
            let body = format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"den_filter_titles","arguments":{{"sel":["{sel}"],"limit":24}}}}}}"#
            );
            Request::post(format!("{origin}/mcp"))
                .header("authorization", token.as_str())
                .header("content-type", "application/json")
                .body(Full::new(Bytes::from(body)))
                .unwrap()
        }
    };
    let direct = || {
        Request::get(format!("http://127.0.0.1:{atlas}/index/filter/all/titles.json?sel=genre:80"))
            .body(Full::new(Bytes::new()))
            .unwrap()
    };
    // Warm both paths: the pooled connections, the schema read.
    timed(&client, direct, 200).await;
    timed(&client, call("genre:80"), 200).await;

    let (atlas_p50, atlas_p99) = report("atlas directly", &timed(&client, direct, CALLS).await);
    let (mcp_p50, mcp_p99) = report(
        "den_filter_titles via den-mcp, atlas each call",
        &timed(&client, call("genre:80"), CALLS).await,
    );
    report("den_filter_titles via den-mcp, cached", &timed(&client, call("genre:18"), CALLS).await);
    println!(
        "overhead on top of atlas                       p50 {:>7.3} ms   p99 {:>7.3} ms",
        mcp_p50 - atlas_p50,
        mcp_p99 - atlas_p99
    );

    let started = Instant::now();
    let workers: Vec<_> = (0..CONCURRENT)
        .map(|_| {
            let client = client.clone();
            let call = call("genre:80");
            tokio::spawn(async move { timed(&client, call, CALLS / CONCURRENT).await })
        })
        .collect();
    let mut all = Vec::new();
    for w in workers {
        all.extend(w.await.unwrap());
    }
    all.sort();
    let rate = all.len() as f64 / started.elapsed().as_secs_f64();
    report(&format!("{CONCURRENT} concurrent callers ({rate:.0} calls/s)"), &all);
    println!("RSS after {} calls                           {} KB", CALLS * 4 + 400, rss_kb(child.id()));
    let _ = child.kill();
    let _ = child.wait();
}
