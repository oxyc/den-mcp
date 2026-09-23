//! Who may call `/mcp`: the bearer of an access token den-edge signed (EdDSA JWS, RFC 9068's `at+jwt`), checked
//! here with its public key alone — no call to den-edge per request — and a per-token rate limit.
//!
//! The token names an opaque session (`sub`), never a library or a grant. den-edge refuses a revoked session's
//! tokens at its relay; here a token stands until its `exp`, which den-edge keeps short and never past a guest's
//! end of access.

use ed25519_dalek::{Signature, VerifyingKey};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

/// Seconds of clock skew allowed between den-edge and this server.
const LEEWAY_SECS: u64 = 30;
/// The scope every token for this server carries.
pub const SCOPE: &str = "den:search";

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Unpadded base64url. Only the tests write any: den-edge signs the tokens.
#[cfg(test)]
pub fn b64url(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// Each byte's base64url value, or 0xFF for a byte that is none: one lookup a character rather than a search.
const DECODE: [u8; 256] = {
    let mut table = [0xFF; 256];
    let mut i = 0;
    while i < 64 {
        table[B64[i] as usize] = i as u8;
        i += 1;
    }
    table
};

/// Unpadded base64url, canonical only (leftover bits zero).
pub fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for b in s.bytes() {
        let value = DECODE[b as usize];
        if value == 0xFF {
            return None;
        }
        acc = acc << 6 | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    (acc == 0).then_some(out)
}

/// A verified token's caller.
#[derive(Debug, Clone, PartialEq)]
pub struct Caller {
    /// The session, which the rate limit is kept per.
    pub session: String,
    /// `member` or `guest`, for the log and the metrics.
    pub kind: String,
}

/// Why a token was refused. Said in `WWW-Authenticate`'s `error_description`, which names no secret.
#[derive(Debug, PartialEq)]
pub enum Refused {
    Missing,
    Invalid(&'static str),
}

/// The bearer token in an `Authorization` header. The scheme is case-insensitive (RFC 9110 §11.1).
fn bearer(header: Option<&str>) -> Result<&str, Refused> {
    header
        .and_then(|h| h.trim_start().split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token.trim())
        .ok_or(Refused::Missing)
}

/// The `Authorization: Bearer` token, checked: signature by one of `keys`, issuer, audience, expiry, scope. The
/// server goes through `Verified`, which keeps what this checks; the tests check tokens with it directly.
#[cfg(test)]
pub fn verify(
    header: Option<&str>,
    keys: &[VerifyingKey],
    issuer: &str,
    audience: &str,
    now: u64,
) -> Result<Caller, Refused> {
    check(bearer(header)?, keys, issuer, audience, now).map(|(caller, _)| caller)
}

/// Tokens already verified, and when each expires: a client presents the same token for up to fifteen minutes, so a
/// signature is checked once, not on every call. Keyed by the whole token, so nothing but that exact token (its claims
/// and signature both) is taken from here. Bounded; the issuer, audience and keys are the server's own and never
/// change while it runs.
#[derive(Default)]
pub struct Verified {
    seen: Mutex<HashMap<String, (Caller, u64)>>,
}

/// The most verified tokens kept.
const VERIFIED_KEPT: usize = 1024;

impl Verified {
    pub fn verify(
        &self,
        header: Option<&str>,
        keys: &[VerifyingKey],
        issuer: &str,
        audience: &str,
        now: u64,
    ) -> Result<Caller, Refused> {
        let token = bearer(header)?;
        let fresh = |exp: u64| now <= exp + LEEWAY_SECS;
        if let Some((caller, exp)) = self.seen.lock().unwrap_or_else(|e| e.into_inner()).get(token) {
            if fresh(*exp) {
                return Ok(caller.clone());
            }
        }
        let (caller, exp) = check(token, keys, issuer, audience, now)?;
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        if seen.len() >= VERIFIED_KEPT {
            seen.retain(|_, (_, exp)| fresh(*exp));
            if seen.len() >= VERIFIED_KEPT {
                seen.clear();
            }
        }
        seen.insert(token.to_owned(), (caller.clone(), exp));
        Ok(caller)
    }

    #[cfg(test)]
    pub fn kept(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

/// A token checked, and its expiry.
fn check(
    token: &str,
    keys: &[VerifyingKey],
    issuer: &str,
    audience: &str,
    now: u64,
) -> Result<(Caller, u64), Refused> {
    let bad = Refused::Invalid;
    let mut parts = token.split('.');
    let (Some(head), Some(body), Some(sig), None) = (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(bad("malformed"));
    };
    let header: Value =
        b64url_decode(head).and_then(|b| serde_json::from_slice(&b).ok()).ok_or(bad("malformed"))?;
    // The algorithm is pinned, never read from the token: a token that names another is refused outright.
    if header.get("alg").and_then(Value::as_str) != Some("EdDSA") {
        return Err(bad("unsupported alg"));
    }
    // An access token, not some other JWT the same key might sign (RFC 9068 §4): `at+jwt`, or its full media type.
    let typ = header.get("typ").and_then(Value::as_str).unwrap_or("");
    if !typ.eq_ignore_ascii_case("at+jwt") && !typ.eq_ignore_ascii_case("application/at+jwt") {
        return Err(bad("not an access token"));
    }
    let sig: [u8; 64] =
        b64url_decode(sig).and_then(|s| s.try_into().ok()).ok_or(bad("malformed signature"))?;
    let sig = Signature::from_bytes(&sig);
    let signed = &token.as_bytes()[..head.len() + 1 + body.len()];
    if !keys.iter().any(|k| k.verify_strict(signed, &sig).is_ok()) {
        return Err(bad("bad signature"));
    }
    let claims: Value =
        b64url_decode(body).and_then(|b| serde_json::from_slice(&b).ok()).ok_or(bad("malformed claims"))?;
    if claims.get("iss").and_then(Value::as_str) != Some(issuer) {
        return Err(bad("wrong issuer"));
    }
    let aud_ok = match claims.get("aud") {
        Some(Value::String(a)) => a == audience,
        Some(Value::Array(list)) => list.iter().any(|a| a.as_str() == Some(audience)),
        _ => false,
    };
    if !aud_ok {
        return Err(bad("wrong audience"));
    }
    let exp = claims.get("exp").and_then(Value::as_u64).ok_or(bad("no expiry"))?;
    if now > exp + LEEWAY_SECS {
        return Err(bad("expired"));
    }
    if claims.get("iat").and_then(Value::as_u64).is_some_and(|iat| iat > now + LEEWAY_SECS) {
        return Err(bad("issued in the future"));
    }
    let scoped =
        claims.get("scope").and_then(Value::as_str).is_some_and(|s| s.split(' ').any(|s| s == SCOPE));
    if !scoped {
        return Err(bad("missing scope"));
    }
    let session = claims
        .get("sub")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 64)
        .ok_or(bad("no subject"))?;
    let kind = match claims.get("kind").and_then(Value::as_str) {
        Some("guest") => "guest",
        _ => "member",
    };
    Ok((Caller { session: session.to_owned(), kind: kind.to_owned() }, exp))
}

/// A token bucket per session: `burst` calls at once, refilled at `per_minute`.
pub struct RateLimit {
    per_minute: f64,
    burst: f64,
    buckets: Mutex<HashMap<String, (f64, Instant)>>,
}

impl RateLimit {
    pub fn new(per_minute: u32, burst: u32) -> Self {
        RateLimit { per_minute: f64::from(per_minute), burst: f64::from(burst), buckets: Mutex::default() }
    }

    /// Take one call from `session`'s bucket, or say how many seconds until one is back.
    pub fn take(&self, session: &str, now: Instant) -> Result<(), u64> {
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        // A full bucket is the same as no bucket, so an idle session costs nothing to forget.
        if buckets.len() > 4096 {
            let (rate, burst) = (self.per_minute / 60.0, self.burst);
            buckets.retain(|_, (tokens, at)| *tokens + now.duration_since(*at).as_secs_f64() * rate < burst);
        }
        let (tokens, at) = buckets.entry(session.to_owned()).or_insert((self.burst, now));
        let refilled =
            (*tokens + now.duration_since(*at).as_secs_f64() * self.per_minute / 60.0).min(self.burst);
        *at = now;
        if refilled >= 1.0 {
            *tokens = refilled - 1.0;
            Ok(())
        } else {
            *tokens = refilled;
            Err(((1.0 - refilled) * 60.0 / self.per_minute).ceil().max(1.0) as u64)
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    pub fn key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    /// A token as den-edge mints one.
    pub fn token(key: &SigningKey, claims: &Value) -> String {
        let head = b64url(br#"{"alg":"EdDSA","typ":"at+jwt"}"#);
        let body = b64url(claims.to_string().as_bytes());
        let signed = format!("{head}.{body}");
        let sig = key.sign(signed.as_bytes());
        format!("{signed}.{}", b64url(&sig.to_bytes()))
    }

    pub fn claims(now: u64) -> Value {
        serde_json::json!({
            "iss": "https://den.example", "aud": "https://den.example/mcp", "sub": "s1", "kind": "guest",
            "scope": "den:search", "iat": now, "exp": now + 900, "client_id": "c1",
        })
    }

    fn check(token: &str, now: u64) -> Result<Caller, Refused> {
        verify(
            Some(&format!("Bearer {token}")),
            &[key().verifying_key()],
            "https://den.example",
            "https://den.example/mcp",
            now,
        )
    }

    #[test]
    fn base64url_round_trips_and_refuses_what_is_not_canonical() {
        for len in 0..40 {
            let bytes: Vec<u8> = (0..len as u8).map(|b| b.wrapping_mul(37)).collect();
            assert_eq!(b64url_decode(&b64url(&bytes)).unwrap(), bytes);
        }
        assert_eq!(b64url_decode("A"), None);
        assert_eq!(b64url_decode("AB"), None, "leftover bits set");
        assert_eq!(b64url_decode("a+b/"), None, "not the url alphabet");
    }

    #[test]
    fn a_token_den_edge_signed_is_the_session_it_names() {
        let now = 1_800_000_000;
        let caller = check(&token(&key(), &claims(now)), now).unwrap();
        assert_eq!(caller, Caller { session: "s1".into(), kind: "guest".into() });
    }

    #[test]
    fn every_way_a_token_can_be_wrong_is_refused() {
        let now = 1_800_000_000;
        let with = |f: &dyn Fn(&mut Value)| {
            let mut c = claims(now);
            f(&mut c);
            check(&token(&key(), &c), now)
        };
        assert_eq!(
            with(&|c| c["iss"] = "https://evil.example".into()),
            Err(Refused::Invalid("wrong issuer"))
        );
        assert_eq!(
            with(&|c| c["aud"] = "https://den.example/other".into()),
            Err(Refused::Invalid("wrong audience"))
        );
        assert!(with(&|c| c["aud"] = serde_json::json!(["x", "https://den.example/mcp"])).is_ok());
        assert_eq!(with(&|c| c["exp"] = (now - 31).into()), Err(Refused::Invalid("expired")));
        assert!(with(&|c| c["exp"] = (now - 29).into()).is_ok(), "within the skew allowed");
        assert_eq!(with(&|c| c["scope"] = "other".into()), Err(Refused::Invalid("missing scope")));
        assert_eq!(with(&|c| c["sub"] = "".into()), Err(Refused::Invalid("no subject")));
        let other = SigningKey::from_bytes(&[8u8; 32]);
        assert_eq!(check(&token(&other, &claims(now)), now), Err(Refused::Invalid("bad signature")));
        // A token naming another algorithm is refused before anything else is read from it.
        let good = token(&key(), &claims(now));
        let rest = good.split_once('.').unwrap().1;
        let none = format!("{}.{rest}", b64url(br#"{"alg":"none"}"#));
        assert_eq!(check(&none, now), Err(Refused::Invalid("unsupported alg")));
        // A claim changed after signing.
        let mut parts: Vec<&str> = good.split('.').collect();
        let forged_body = b64url(claims(now).to_string().replace("guest", "member").as_bytes());
        parts[1] = &forged_body;
        assert_eq!(check(&parts.join("."), now), Err(Refused::Invalid("bad signature")));
        assert_eq!(verify(None, &[key().verifying_key()], "i", "a", now), Err(Refused::Missing));
        assert_eq!(check("a.b", now), Err(Refused::Invalid("malformed")));
        // Signed by the right key but not an access token: a JWT of another type, or one that names none.
        for head in [br#"{"alg":"EdDSA","typ":"JWT"}"#.as_slice(), br#"{"alg":"EdDSA"}"#] {
            let signed = format!("{}.{}", b64url(head), b64url(claims(now).to_string().as_bytes()));
            let sig = b64url(&key().sign(signed.as_bytes()).to_bytes());
            assert_eq!(check(&format!("{signed}.{sig}"), now), Err(Refused::Invalid("not an access token")));
        }
    }

    #[test]
    fn the_bearer_scheme_is_read_in_any_case() {
        let now = 1_800_000_000;
        let token = token(&key(), &claims(now));
        for scheme in ["Bearer", "bearer", "BEARER", "BeArEr"] {
            let header = format!("{scheme} {token}");
            let caller = verify(
                Some(&header),
                &[key().verifying_key()],
                "https://den.example",
                "https://den.example/mcp",
                now,
            );
            assert!(caller.is_ok(), "{scheme}");
        }
        let basic = format!("Basic {token}");
        assert_eq!(
            verify(Some(&basic), &[key().verifying_key()], "i", "a", now),
            Err(Refused::Missing),
            "another scheme is no bearer token"
        );
    }

    /// A token den-edge's `oauth::access_token` signed (its `the_token_shape_den_mcp_holds_as_a_vector`), with the
    /// same test key: what den-edge mints is what this verifies.
    #[test]
    fn a_token_den_edge_signs_verifies() {
        const SIGNED: &str = "eyJhbGciOiJFZERTQSIsImtpZCI6ImZlODEyYzEyZjNhYjRjZTYiLCJ0eXAiOiJhdCtqd3QifQ.\
            eyJhdWQiOiJodHRwczovL2Rlbi5leGFtcGxlL21jcCIsImNsaWVudF9pZCI6ImMxIiwiZXhwIjoxODAwMDAwOTAwLCJpYXQiOjE4MDAw\
            MDAwMDAsImlzcyI6Imh0dHBzOi8vZGVuLmV4YW1wbGUiLCJqdGkiOiI0YWM4Y2MyMmU2MDkyNTYwYWFhZTJiODY2OWY3ZTE3NyIsImtp\
            bmQiOiJndWVzdCIsInNjb3BlIjoiZGVuOnNlYXJjaCIsInN1YiI6IjAxMjM0NTY3ODlhYmNkZWYwMTIzNDU2Nzg5YWJjZGVmIn0.\
            A5trlYJ9ziYFBg93YOOqg9L1-3reCylq-cKRPSFdrWnz4moZUHDByGU-8mzczQXd5HctPzZas7UPzJPqZhwtCw";
        let caller = check(SIGNED, 1_800_000_100).unwrap();
        assert_eq!(
            caller,
            Caller { session: "0123456789abcdef0123456789abcdef".into(), kind: "guest".into() }
        );
        assert_eq!(check(SIGNED, 1_800_000_900 + 31), Err(Refused::Invalid("expired")));
    }

    #[test]
    fn a_verified_token_is_kept_until_it_expires_and_only_that_token() {
        let now = 1_800_000_000;
        let verified = Verified::default();
        let (iss, aud) = ("https://den.example", "https://den.example/mcp");
        let good = format!("Bearer {}", token(&key(), &claims(now)));
        assert!(verified.verify(Some(&good), &[key().verifying_key()], iss, aud, now).is_ok());
        // Kept: the same token again needs no key at all — its signature is not checked a second time.
        assert!(verified.verify(Some(&good), &[], iss, aud, now + 60).is_ok());
        // Another token, even one differing only in its claims, is checked from scratch.
        let mut other = claims(now);
        other["sub"] = "s2".into();
        let other = format!("Bearer {}", token(&key(), &other));
        assert_eq!(verified.verify(Some(&other), &[], iss, aud, now), Err(Refused::Invalid("bad signature")));
        // Past its expiry, a kept token is checked again, and refused.
        let later = now + 900 + LEEWAY_SECS + 1;
        assert_eq!(
            verified.verify(Some(&good), &[key().verifying_key()], iss, aud, later),
            Err(Refused::Invalid("expired"))
        );
        // Bounded, however many tokens come.
        for n in 0..(VERIFIED_KEPT as u64 + 50) {
            let mut c = claims(now);
            c["sub"] = format!("s{n}").into();
            let t = format!("Bearer {}", token(&key(), &c));
            verified.verify(Some(&t), &[key().verifying_key()], iss, aud, now).unwrap();
        }
        assert!(verified.kept() <= VERIFIED_KEPT);
    }

    #[test]
    fn a_session_gets_its_burst_then_its_rate() {
        let limit = RateLimit::new(60, 3);
        let t0 = Instant::now();
        for _ in 0..3 {
            assert_eq!(limit.take("s", t0), Ok(()));
        }
        assert_eq!(limit.take("s", t0), Err(1));
        assert_eq!(limit.take("other", t0), Ok(()), "per session");
        assert_eq!(limit.take("s", t0 + std::time::Duration::from_millis(1000)), Ok(()), "one a second");
    }
}
