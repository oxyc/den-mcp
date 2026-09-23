//! A development token, for calling a local den-mcp by hand without den-edge:
//!
//!   cargo run --example mint -- https://den.example
//!
//! prints `TOKEN_PUBLIC_KEYS=<key>` for the server and a 15-minute bearer token for that origin. The signing key
//! is a fixed test key, so the token means nothing to a server configured with den-edge's real key.

use ed25519_dalek::{Signer, SigningKey};

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn b64url(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

pub fn dev_key() -> SigningKey {
    SigningKey::from_bytes(&[42u8; 32])
}

pub fn token(key: &SigningKey, origin: &str, sub: &str, ttl: u64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let claims = serde_json::json!({
        "iss": origin, "aud": format!("{origin}/mcp"), "sub": sub, "kind": "member",
        "scope": "den:search", "iat": now, "exp": now + ttl, "client_id": "dev",
    });
    let signed =
        format!("{}.{}", b64url(br#"{"alg":"EdDSA","typ":"at+jwt"}"#), b64url(claims.to_string().as_bytes()));
    let sig = key.sign(signed.as_bytes()).to_bytes();
    format!("{signed}.{}", b64url(&sig))
}

#[allow(dead_code)]
fn main() {
    let origin = std::env::args().nth(1).unwrap_or_else(|| "http://localhost:8096".into());
    let key = dev_key();
    println!("TOKEN_PUBLIC_KEYS={}", b64url(key.verifying_key().as_bytes()));
    println!("{}", token(&key, origin.trim_end_matches('/'), "dev", 900));
}
