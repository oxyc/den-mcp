//! Peak memory of the read tools, for den-mcp's 32 MB container:
//!
//!   cargo build --release && cargo run --release --example readload -- real 3
//!
//! `real` is a projection about the size of a household's (~64 KB sealed); `large` is 5,000 watchlist entries, 500 in
//! Continue Watching and 50,000 Seen plays. The number is how many read calls are made at once (1, 3: one of each
//! tool, or 8). Starts a stub atlas and a stub den-edge answering that projection, and the release binary against
//! them with a read token; prints the sealed size and the process's peak resident set (`ru_maxrss`), from a fresh
//! process every run so one scenario's peak never carries into the next.

#[path = "mint.rs"]
mod mint;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde_json::{json, Value};
use std::time::Duration;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../testdata/assistant-v1.json")).unwrap()
}

fn unhex32(text: &str) -> [u8; 32] {
    (0..32)
        .map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).unwrap())
        .collect::<Vec<_>>()
        .try_into()
        .unwrap()
}

/// The sealed answer of `GET /assistant/projection` for lists of these lengths, and its size in characters.
fn projection(watchlist: usize, continuing: usize, seen: usize, pool: usize) -> (Value, usize) {
    let f = fixture();
    let mut x = 88_172_645_463_325_252u64;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let titles: Vec<(&str, u64)> =
        (0..pool).map(|_| (if next() % 10 < 6 { "movie" } else { "tv" }, 1 + next() % 1_000_000)).collect();
    let title = |n: u64| json!({ "type": titles[n as usize % pool].0, "id": titles[n as usize % pool].1 });
    let is_tv = |n: u64| titles[n as usize % pool].0 == "tv";
    let at = |n: u64| 1_789_000_000_000u64 - n * 3_600_000;
    let watchlist: Vec<Value> =
        (0..watchlist as u64).map(|i| json!({ "title": title(next()), "addedAt": at(i) })).collect();
    let continuing: Vec<Value> = (0..continuing as u64)
        .map(|i| {
            let n = next();
            let mut e = json!({ "title": title(n), "action": "resume", "fraction": 0.5, "at": at(i) });
            if is_tv(n) {
                e["season"] = json!(1 + n % 8);
                e["episode"] = json!(1 + n % 20);
            }
            e
        })
        .collect();
    let seen: Vec<Value> = (0..seen as u64)
        .map(|i| {
            let n = next();
            let mut e = json!({ "title": title(n), "at": at(i) });
            if is_tv(n) {
                e["season"] = json!(1 + n % 8);
                e["episode"] = json!(1 + n % 20);
            }
            e
        })
        .collect();
    let view = json!({ "v": 1, "library": f["library"], "grant": f["grant"]["id"], "at": 1_790_000_000_000u64,
                       "head": 1, "watchlist": watchlist, "continue": continuing, "seen": seen });
    let key = den_assistant::b64url_decode(f["read"]["readKey"].as_str().unwrap()).unwrap();
    let sealed = den_assistant::seal_projection(&key.try_into().unwrap(), &view, &[7; 32]).unwrap();
    let chars = sealed.parts.iter().map(String::len).sum();
    (
        json!({ "library": f["library"], "grant": f["grant"]["id"], "set": sealed.set, "parts": sealed.parts,
                "storedAt": 1_790_000_001_000u64 }),
        chars,
    )
}

async fn stub(answer: impl Fn(&str) -> (u16, String) + Clone + Send + Sync + 'static) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let answer = answer.clone();
            let service = hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
                let (status, body) = answer(req.uri().path());
                async move {
                    // A request's body is read before it is answered, as a server must.
                    let _ = req.into_body().collect().await;
                    Ok::<_, std::convert::Infallible>(
                        hyper::Response::builder()
                            .status(status)
                            .header("content-type", "application/json")
                            .header("cache-control", "public, max-age=3600")
                            .body(Full::new(Bytes::from(body)))
                            .unwrap(),
                    )
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

/// The waited-for child's peak resident set (KB) and CPU time (ms, user + system).
fn child_usage() -> (u64, f64) {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) };
    let max = usage.ru_maxrss as u64;
    let secs = |t: libc::timeval| t.tv_sec as f64 * 1000.0 + t.tv_usec as f64 / 1000.0;
    (if cfg!(target_os = "macos") { max / 1024 } else { max }, secs(usage.ru_utime) + secs(usage.ru_stime))
}

fn rss_kb(pid: u32) -> u64 {
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &pid.to_string()]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let size = args.next().unwrap_or_else(|| "real".into());
    let concurrent: usize = args.next().and_then(|n| n.parse().ok()).unwrap_or(3);
    let rounds: usize = args.next().and_then(|n| n.parse().ok()).unwrap_or(1);
    let (answer, chars) = match size.as_str() {
        "large" => projection(5_000, 500, 50_000, 12_000),
        _ => projection(400, 40, 6_000, 2_000),
    };
    println!("{size}: {} sealed characters in {} part(s)", chars, answer["parts"].as_array().unwrap().len());
    let answer = answer.to_string();
    let edge = stub(move |_| (200, answer.clone())).await;
    let atlas = stub(|path| {
        if path == "/index/schema.json" {
            (200, r#"{"tmdb":{"filterKinds":["rating","character"]}}"#.into())
        } else if path == "/recommend" {
            // The ranking `den_recommend` asks for: 11 slides with the scorer's terms.
            let slides: Vec<Value> = (0..11)
                .map(|i| json!({ "type": "movie", "id": 9000 + i, "why": { "score": 0.5, "reason": "similar" } }))
                .collect();
            (200, json!({ "slides": slides }).to_string())
        } else {
            (200, r#"{"type":"movie","id":1,"indexed":false}"#.into())
        }
    })
    .await;
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let origin = format!("http://127.0.0.1:{port}");
    let f = fixture();
    let key = mint::dev_key();
    let mut child =
        std::process::Command::new(env!("CARGO_MANIFEST_DIR").to_owned() + "/target/release/den-mcp")
            .env("PORT", port.to_string())
            .env("ATLAS_URL", format!("http://127.0.0.1:{atlas}"))
            .env("PUBLIC_ORIGIN", &origin)
            .env("TOKEN_PUBLIC_KEYS", mint::b64url(key.verifying_key().as_bytes()))
            .env("MCP_WRITE_KEY", mint::b64url(&unhex32(f["mcp"]["seed"].as_str().unwrap())))
            .env("EDGE_URL", format!("http://127.0.0.1:{edge}"))
            .env("RATE_PER_MINUTE", "1000000")
            .env("RATE_BURST", "1000000")
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("build the release binary first: cargo build --release");
    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new()).build_http();
    loop {
        let health = Request::get(format!("{origin}/health")).body(Full::new(Bytes::new())).unwrap();
        if client.request(health).await.is_ok_and(|r| r.status().is_success()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // A read token for the vectors' session: `dr` is the vectors' claim, sealed to the vectors' token key.
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let claims = json!({ "iss": origin, "aud": format!("{origin}/mcp"), "sub": f["sub"], "kind": "member",
                         "scope": "den:search den:library.read", "dr": f["read"]["claim"],
                         "iat": now, "exp": now + 3600, "client_id": "load" });
    let signed = format!(
        "{}.{}",
        mint::b64url(br#"{"alg":"EdDSA","typ":"at+jwt"}"#),
        mint::b64url(claims.to_string().as_bytes())
    );
    use ed25519_dalek::Signer;
    let token = format!("Bearer {signed}.{}", mint::b64url(&key.sign(signed.as_bytes()).to_bytes()));

    tokio::time::sleep(Duration::from_millis(300)).await;
    println!("idle RSS {} KB", rss_kb(child.id()));

    // Without a fourth argument the calls cycle through the three lists; `recommend` makes every one `den_recommend`.
    let tools = if args.next().as_deref() == Some("recommend") {
        ["den_recommend"; 3]
    } else {
        ["den_watchlist", "den_continue_watching", "den_history"]
    };
    let mut failed = 0;
    for _ in 0..rounds {
        let calls: Vec<_> = (0..concurrent)
            .map(|i| {
                let (client, token, origin) = (client.clone(), token.clone(), origin.clone());
                let tool = tools[i % 3];
                tokio::spawn(async move {
                    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                                       "params": { "name": tool, "arguments": { "limit": 20 } } });
                    let req = Request::post(format!("{origin}/mcp"))
                        .header("authorization", token.as_str())
                        .header("content-type", "application/json")
                        .body(Full::new(Bytes::from(body.to_string())))
                        .unwrap();
                    let resp = client.request(req).await.expect("den-mcp is alive");
                    let body = resp.into_body().collect().await.unwrap().to_bytes();
                    let answer: Value = serde_json::from_slice(&body).unwrap();
                    assert!(answer["result"]["isError"].is_null(), "{answer}");
                })
            })
            .collect();
        for call in calls {
            failed += usize::from(call.await.is_err());
        }
    }
    let alive = child.try_wait().unwrap().is_none();
    let after = rss_kb(child.id());
    let _ = child.kill();
    let _ = child.wait();
    let (peak, cpu_ms) = child_usage();
    println!(
        "{concurrent} concurrent x {rounds} round(s): peak RSS {peak} KB, after {after} KB, CPU {:.2} ms a call{}",
        cpu_ms / (concurrent * rounds) as f64,
        if alive && failed == 0 { "" } else { "  (the process died or a call failed)" }
    );
}
