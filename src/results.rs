//! Play results (den-spec `wire/play-results-v1.md`): what a TV says back about a play this server queued —
//! preparing, playing, or why not. Each result is sealed under a key derived from the request's own random id, which
//! this server made and sealed into the request; den-edge stores them opaque by an address derived from the same id.
//! Nothing here needs a library key: the id is the whole of the secret.

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit};
use hkdf::Hkdf;
use serde_json::Value;
use sha2::Sha256;

const SALT: &[u8] = b"den/play-result/v1";
/// A nonce, a tag and at least one byte.
const MIN_SEALED: usize = 12 + 16 + 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Preparing,
    Failed,
    Playing,
}

impl State {
    /// §3 *Order*: playing over failed over preparing.
    fn rank(self) -> u8 {
        match self {
            State::Preparing => 1,
            State::Failed => 2,
            State::Playing => 3,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PlayResult {
    pub state: State,
    /// Only with `failed`: one of §3's reasons, an unknown one read as `failed`.
    pub reason: Option<&'static str>,
    pub at: u64,
}

const REASONS: [&str; 6] = ["not_allowed", "no_source", "no_trailer", "expired", "superseded", "failed"];

/// A request id's 16 bytes, from its 32 lowercase hex.
fn secret(id: &str) -> Option<[u8; 16]> {
    if id.len() != 32 || !id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&id[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

fn derive(id: &[u8; 16], info: &[u8], out: &mut [u8]) {
    Hkdf::<Sha256>::new(Some(SALT), id).expand(info, out).expect("an HKDF-SHA256 output this short");
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Where den-edge keeps request `id`'s results: 32 lowercase hex, or `None` for an id that is not one.
pub fn address(id: &str) -> Option<String> {
    let id = secret(id)?;
    let mut address = [0u8; 16];
    derive(&id, b"address", &mut address);
    Some(hex(&address))
}

/// The results among `sealed` that open as request `id`'s; anything else counts for nothing.
pub fn open(id: &str, sealed: &[String]) -> Vec<PlayResult> {
    let (Some(secret), Some(address)) = (secret(id), address(id)) else { return Vec::new() };
    let mut key = zeroize::Zeroizing::new([0u8; 32]);
    derive(&secret, b"key", &mut *key);
    let cipher = Aes256Gcm::new((&*key).into());
    let mut aad = SALT.to_vec();
    aad.push(0);
    aad.extend_from_slice(address.as_bytes());
    sealed
        .iter()
        .filter_map(|sealed| {
            let bytes = den_assistant::b64url_decode(sealed).filter(|b| b.len() >= MIN_SEALED)?;
            let nonce: &[u8; 12] = bytes[..12].try_into().ok()?;
            let plain = cipher.decrypt(nonce.into(), Payload { msg: &bytes[12..], aad: &aad }).ok()?;
            let body: Value = serde_json::from_slice(&plain).ok()?;
            if body.get("v").and_then(Value::as_u64) != Some(1) {
                return None;
            }
            let at = body.get("at")?.as_u64()?;
            let state = match body.get("state")?.as_str()? {
                "preparing" => State::Preparing,
                "playing" => State::Playing,
                "failed" => State::Failed,
                _ => return None,
            };
            let reason = (state == State::Failed).then(|| {
                let said = body.get("reason").and_then(Value::as_str).unwrap_or("failed");
                REASONS.into_iter().find(|r| *r == said).unwrap_or("failed")
            });
            Some(PlayResult { state, reason, at })
        })
        .collect()
}

/// `body` (a result's JSON) sealed for request `id` as a TV seals it, padded to 128 bytes: for the tests' TV.
#[cfg(test)]
pub fn seal(id: &str, body: &Value, nonce: [u8; 12]) -> String {
    let secret = secret(id).expect("a request id");
    let address = address(id).expect("a request id");
    let mut key = [0u8; 32];
    derive(&secret, b"key", &mut key);
    let mut aad = SALT.to_vec();
    aad.push(0);
    aad.extend_from_slice(address.as_bytes());
    let plaintext = format!("{:<128}", body.to_string());
    let sealed = Aes256Gcm::new((&key).into())
        .encrypt((&nonce).into(), Payload { msg: plaintext.as_bytes(), aad: &aad })
        .expect("a result seals");
    den_assistant::b64url(&[nonce.as_slice(), &sealed].concat())
}

/// §3 *Order*: the highest state by rank, then the latest `at`; so a reordered or replayed result never moves back.
pub fn strongest(results: &[PlayResult]) -> Option<&PlayResult> {
    results.iter().max_by_key(|r| (r.state.rank(), r.at))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vectors() -> Value {
        serde_json::from_str(include_str!("../testdata/play-results-v1.json")).unwrap()
    }

    #[test]
    fn the_vectors_open_and_rank_as_den_spec_says() {
        let v = vectors();
        let id = v["id"].as_str().unwrap();
        assert_eq!(address(id).as_deref(), v["address"].as_str());
        let sealed: Vec<String> = v["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["sealed"].as_str().unwrap().to_owned())
            .collect();
        let opened = open(id, &sealed);
        assert_eq!(
            opened,
            [
                PlayResult { state: State::Preparing, reason: None, at: 1_789_000_061_000 },
                PlayResult { state: State::Playing, reason: None, at: 1_789_000_069_000 },
                PlayResult { state: State::Failed, reason: Some("no_source"), at: 1_789_000_066_000 },
            ]
        );
        assert_eq!(strongest(&opened).unwrap().state, State::Playing);
        assert_eq!(strongest(&opened[..1]).unwrap().state, State::Preparing);
        // Another request's key opens none of them, and junk is nothing.
        assert!(open("ffeeddccbbaa99887766554433221101", &sealed).is_empty());
        assert!(open(id, &["AAAA".into(), "not base64!".into()]).is_empty());
        assert_eq!(address("FFEEDDCCBBAA99887766554433221100"), None);
        // The tests' TV seals byte for byte as den-spec's vectors.
        for result in v["results"].as_array().unwrap() {
            let plaintext = result["plaintext"].as_str().unwrap();
            let body: Value = serde_json::from_str(plaintext).unwrap();
            let nonce: [u8; 12] = (0..12)
                .map(|i| {
                    u8::from_str_radix(&result["nonce"].as_str().unwrap()[2 * i..2 * i + 2], 16).unwrap()
                })
                .collect::<Vec<_>>()
                .try_into()
                .unwrap();
            assert_eq!(seal(id, &body, nonce), result["sealed"].as_str().unwrap());
        }
    }
}
