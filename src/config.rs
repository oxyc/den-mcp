//! Configuration, from the environment. Unset and empty both mean "not configured", the rule every den addon uses.

use std::time::Duration;

pub struct Config {
    pub port: u16,
    /// den-atlas, plain HTTP on the LAN: `http://host:port`, no trailing slash.
    pub atlas: Atlas,
    /// The public origin clients reach this server on (den-edge relays `/mcp` there): the resource an access
    /// token must name, and where the protected-resource metadata points.
    pub public_origin: String,
    /// Where Den Web's pages live, for the link every result carries. Defaults to `public_origin`.
    pub web_origin: String,
    /// The authorization server whose tokens are accepted: den-edge's `OAUTH_ISSUER`.
    pub issuer: String,
    /// Ed25519 public keys a token may be signed with; two during a rotation.
    pub token_keys: Vec<ed25519_dalek::VerifyingKey>,
    /// Tool calls one token may make a minute, and how many at once before the rate applies.
    pub rate_per_minute: u32,
    pub rate_burst: u32,
    pub metrics_token: Option<String>,
    pub log_requests: bool,
    /// Extra `Origin`s a browser-based client may call from. A request with no `Origin` (a connector's server)
    /// is always let through; one naming another site is refused (the MCP spec's DNS-rebinding guard).
    pub allowed_origins: Vec<String>,
    /// Paid open-facet classification. Absent unless a deployment deliberately opts in.
    pub open_facets: Option<OpenFacets>,
}

pub struct OpenFacets {
    pub endpoint: String,
    pub api_key: String,
    /// A resolved model id, never a moving `*-latest` alias, so persisted decisions retain their meaning.
    pub model: String,
    pub corpus: String,
    pub state_dir: std::path::PathBuf,
    pub max_candidates: usize,
    pub max_calls: u64,
    pub max_input_tokens: u64,
    pub dollars_per_billion_input_tokens: u64,
    pub max_cost_nano_usd: u64,
    pub timeout: Duration,
}

pub struct Atlas {
    pub base: String,
    pub timeout: Duration,
}

fn env_opt(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

/// `http(s)://host[:port]`, lower-cased, without a trailing slash; `None` for anything with a path, a query or
/// credentials, which would read differently from how it is used.
pub fn origin(value: &str) -> Option<String> {
    let o = value.trim().trim_end_matches('/').to_ascii_lowercase();
    let (scheme, host) = o.split_once("://")?;
    let ok = matches!(scheme, "http" | "https")
        && !host.is_empty()
        && !host.contains(['/', '?', '#', '@', ' ', ',', ';']);
    ok.then_some(o)
}

/// An unpadded base64url Ed25519 public key (32 bytes).
pub fn public_key(value: &str) -> Option<ed25519_dalek::VerifyingKey> {
    let bytes: [u8; 32] = crate::auth::b64url_decode(value.trim())?.try_into().ok()?;
    ed25519_dalek::VerifyingKey::from_bytes(&bytes).ok()
}

impl Config {
    pub fn from_env() -> Result<Config, String> {
        let atlas =
            env_opt("ATLAS_URL").ok_or("ATLAS_URL is required (den-atlas, e.g. http://den-atlas:8080)")?;
        let atlas = origin(&atlas)
            .filter(|o| o.starts_with("http://"))
            .ok_or("ATLAS_URL must be a bare http:// origin on the LAN")?;
        let public_origin = env_opt("PUBLIC_ORIGIN")
            .and_then(|v| origin(&v))
            .ok_or("PUBLIC_ORIGIN is required: the https origin clients reach /mcp on")?;
        let web_origin =
            env_opt("WEB_ORIGIN").and_then(|v| origin(&v)).unwrap_or_else(|| public_origin.clone());
        let issuer =
            env_opt("TOKEN_ISSUER").and_then(|v| origin(&v)).unwrap_or_else(|| public_origin.clone());
        let mut token_keys = Vec::new();
        for key in
            env_opt("TOKEN_PUBLIC_KEYS").unwrap_or_default().split(',').filter(|k| !k.trim().is_empty())
        {
            token_keys.push(public_key(key).ok_or("TOKEN_PUBLIC_KEYS: not a base64url Ed25519 public key")?);
        }
        let number = |name: &str, default: u32| env_opt(name).and_then(|v| v.parse().ok()).unwrap_or(default);
        let open_facets = if env_opt("OPEN_FACETS_ENABLED").as_deref() == Some("1") {
            let endpoint = env_opt("OPEN_FACETS_ENDPOINT")
                .unwrap_or_else(|| "https://api.typesafe.ai/v1/systemone".into());
            if !endpoint.starts_with("https://") || endpoint.contains(['?', '#', '@', ' ']) {
                return Err(
                    "OPEN_FACETS_ENDPOINT must be an https URL without credentials, query, or fragment"
                        .into(),
                );
            }
            let model = env_opt("OPEN_FACETS_MODEL").ok_or("OPEN_FACETS_MODEL is required when enabled")?;
            if model.ends_with("-latest") {
                return Err("OPEN_FACETS_MODEL must be a pinned provider model, not a *-latest alias".into());
            }
            let positive = |name: &str| -> Result<u64, String> {
                env_opt(name)
                    .ok_or_else(|| format!("{name} is required when open facets are enabled"))?
                    .parse::<u64>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| format!("{name} must be a positive integer"))
            };
            let max_candidates = positive("OPEN_FACETS_MAX_CANDIDATES")?;
            if max_candidates > 20 {
                return Err("OPEN_FACETS_MAX_CANDIDATES may not exceed 20".into());
            }
            Some(OpenFacets {
                endpoint,
                api_key: env_opt("TYPESAFE_API_KEY").ok_or("TYPESAFE_API_KEY is required when enabled")?,
                model,
                corpus: env_opt("OPEN_FACETS_CORPUS").ok_or("OPEN_FACETS_CORPUS is required when enabled")?,
                state_dir: env_opt("OPEN_FACETS_STATE_DIR")
                    .map(Into::into)
                    .ok_or("OPEN_FACETS_STATE_DIR is required when enabled")?,
                max_candidates: max_candidates as usize,
                max_calls: positive("OPEN_FACETS_MAX_CALLS")?,
                max_input_tokens: positive("OPEN_FACETS_MAX_INPUT_TOKENS")?,
                dollars_per_billion_input_tokens: positive("OPEN_FACETS_DOLLARS_PER_BILLION_INPUT_TOKENS")?,
                max_cost_nano_usd: positive("OPEN_FACETS_MAX_COST_NANO_USD")?,
                timeout: Duration::from_secs(positive("OPEN_FACETS_TIMEOUT_SECONDS")?.min(30)),
            })
        } else {
            None
        };
        Ok(Config {
            port: env_opt("PORT").and_then(|p| p.parse().ok()).unwrap_or(8096),
            atlas: Atlas { base: atlas, timeout: Duration::from_secs(20) },
            public_origin,
            web_origin,
            issuer,
            token_keys,
            rate_per_minute: number("RATE_PER_MINUTE", 120).max(1),
            rate_burst: number("RATE_BURST", 30).max(1),
            metrics_token: env_opt("METRICS_TOKEN"),
            log_requests: env_opt("LOG_REQUESTS").is_some_and(|v| v != "0"),
            allowed_origins: env_opt("ALLOWED_ORIGINS")
                .map(|v| v.split(',').filter_map(origin).collect())
                .unwrap_or_default(),
            open_facets,
        })
    }

    /// The resource an access token must be issued for: this server's `/mcp` on the public origin.
    pub fn resource(&self) -> String {
        format!("{}/mcp", self.public_origin)
    }

    /// Where a client learns which authorization server protects `/mcp` (RFC 9728).
    pub fn resource_metadata_url(&self) -> String {
        format!("{}/.well-known/oauth-protected-resource/mcp", self.public_origin)
    }
}
