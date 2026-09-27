//! Durable, bounded decisions and the deliberately opt-in Jev boundary for open-vocabulary facets.
//!
//! The paid boundary stays small and testable: a caller supplies a bounded, already-preselected population and a
//! `Provider`; the runner derives one stable run key, resumes decisions already on disk, reserves the worst-case
//! call/tokens/cost before every provider invocation, and atomically persists only the bounded decision and
//! accounting. Candidate evidence is never written. The production adapter implements the same trait exercised by
//! deterministic fakes.

use futures_util::future::BoxFuture;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{header, Request, Uri};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const RECORD_VERSION: u32 = 1;
pub const NORMALIZATION_VERSION: &str = "question-normalization-v1";
pub const QUESTION_SCHEMA: &str = r#"{"version":1,"primitive":"choice","options":["yes","no","unknown"]}"#;

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct TitleKey {
    #[serde(rename = "type")]
    pub media_type: String,
    pub id: u64,
}

impl TitleKey {
    pub fn new(media_type: impl Into<String>, id: u64) -> Result<Self, Error> {
        let media_type = media_type.into();
        if !matches!(media_type.as_str(), "movie" | "series") || id == 0 {
            return Err(Error::Invalid("title key is movie|series plus a positive id".into()));
        }
        Ok(Self { media_type, id })
    }

    fn stable(&self) -> String {
        format!("{}:{}", self.media_type, self.id)
    }
}

/// The evidence sent to the provider. `state` may contain source prose and is intentionally absent from `Record`.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub title: TitleKey,
    pub state: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Yes,
    No,
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderAnswer {
    pub decision: Decision,
    /// Provider-reported input usage. It must not exceed the provider's conservative estimate.
    pub input_tokens: u64,
}

/// The only paid boundary. Implementations must estimate conservatively; `run` reserves that estimate first.
pub trait Provider {
    type Failure: fmt::Display;

    fn model(&self) -> &str;
    fn estimate_input_tokens(&self, question: &str, candidate: &Candidate) -> u64;
    fn decide<'a>(
        &'a self,
        question: &str,
        schema: &str,
        candidate: &'a Candidate,
    ) -> BoxFuture<'a, Result<ProviderAnswer, Self::Failure>>;
}

/// The production adapter. Constructing it is free; only `decide` crosses the paid boundary.
pub struct JevProvider {
    client: Client<HttpsConnector<HttpConnector>, Full<bytes::Bytes>>,
    endpoint: Uri,
    authorization: hyper::header::HeaderValue,
    model: String,
    timeout: std::time::Duration,
}

impl JevProvider {
    pub fn new(
        endpoint: String,
        api_key: String,
        model: String,
        timeout: std::time::Duration,
    ) -> Result<Self, Error> {
        if model.ends_with("-latest") {
            return Err(Error::Invalid("the provider model must be pinned, not a *-latest alias".into()));
        }
        let endpoint: Uri =
            endpoint.parse().map_err(|_| Error::Invalid("provider endpoint is not a URL".into()))?;
        let authorization = hyper::header::HeaderValue::from_str(&format!("Bearer {api_key}"))
            .map_err(|_| Error::Invalid("provider API key is not a valid header value".into()))?;
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();
        let client = Client::builder(TokioExecutor::new()).pool_idle_timeout(timeout).build(connector);
        Ok(Self { client, endpoint, authorization, model, timeout })
    }

    fn body(&self, question: &str, candidate: &Candidate) -> Value {
        json!({
            "state": candidate.state,
            "model": self.model,
            "questions": { "facet": {
                "type": "choice",
                "instructions": "Using only the supplied state, decide whether the title has the requested facet. Choose unknown when the state is insufficient.",
                "criteria": {
                    "yes": format!("The title clearly has this facet: {question}"),
                    "no": format!("The title clearly does not have this facet: {question}"),
                    "unknown": "The supplied state does not establish either answer."
                }
            }}
        })
    }
}

impl Provider for JevProvider {
    type Failure = String;

    fn model(&self) -> &str {
        &self.model
    }

    fn estimate_input_tokens(&self, question: &str, candidate: &Candidate) -> u64 {
        // One UTF-8 byte per token plus framing is deliberately conservative for the provider's text model.
        serde_json::to_vec(&self.body(question, candidate)).map_or(u64::MAX, |b| b.len() as u64 + 2_048)
    }

    fn decide<'a>(
        &'a self,
        question: &str,
        _schema: &str,
        candidate: &'a Candidate,
    ) -> BoxFuture<'a, Result<ProviderAnswer, Self::Failure>> {
        let body = self.body(question, candidate);
        Box::pin(async move {
            let request = Request::post(self.endpoint.clone())
                .header(header::AUTHORIZATION, self.authorization.clone())
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::USER_AGENT, "den-mcp")
                .body(Full::new(bytes::Bytes::from(body.to_string())))
                .map_err(|_| "provider request could not be built".to_owned())?;
            let response = tokio::time::timeout(self.timeout, self.client.request(request))
                .await
                .map_err(|_| "provider request timed out".to_owned())?
                .map_err(|_| "provider request failed".to_owned())?;
            if !response.status().is_success() {
                return Err(format!("provider answered HTTP {}", response.status().as_u16()));
            }
            let answer =
                tokio::time::timeout(self.timeout, Limited::new(response.into_body(), 128 * 1024).collect())
                    .await
                    .map_err(|_| "provider answer timed out".to_owned())?
                    .map_err(|_| "provider answer could not be read".to_owned())?
                    .to_bytes();
            let answer: Value =
                serde_json::from_slice(&answer).map_err(|_| "provider answer was not JSON".to_owned())?;
            if answer["model"].as_str() != Some(self.model.as_str()) {
                return Err("provider answered with a different model identity".into());
            }
            let decision = match answer["answers"]["facet"]["choice"].as_str() {
                Some("yes") => Decision::Yes,
                Some("no") => Decision::No,
                Some("unknown") => Decision::Unknown,
                _ => return Err("provider answer did not contain a valid fixed choice".into()),
            };
            let input_tokens = answer["usage"]["input_tokens"]
                .as_u64()
                .ok_or_else(|| "provider answer omitted input usage".to_owned())?;
            Ok(ProviderAnswer { decision, input_tokens })
        })
    }
}

#[derive(Clone, Debug)]
pub struct Limits {
    pub max_candidates: usize,
    pub max_calls: u64,
    pub max_input_tokens: u64,
    /// Whole dollars per billion input tokens (Jev is currently 42). This is a ceiling input, not billing truth.
    pub dollars_per_billion_input_tokens: u64,
    /// Cost ceiling in nano-dollars. At $42/billion, one input token reserves 42 nano-dollars.
    pub max_cost_nano_usd: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct Usage {
    pub calls: u64,
    #[serde(rename = "reservedInputTokens")]
    pub reserved_input_tokens: u64,
    #[serde(rename = "reservedCostNanoUsd")]
    pub reserved_cost_nano_usd: u64,
}

impl Usage {
    fn reserve(&mut self, estimate: u64, limits: &Limits) -> Result<(), Error> {
        let calls = self.calls.checked_add(1).ok_or_else(|| Error::Limit("call count overflow".into()))?;
        let tokens = self
            .reserved_input_tokens
            .checked_add(estimate)
            .ok_or_else(|| Error::Limit("input-token count overflow".into()))?;
        let cost = self
            .reserved_cost_nano_usd
            .checked_add(
                estimate
                    .checked_mul(limits.dollars_per_billion_input_tokens)
                    .ok_or_else(|| Error::Limit("cost overflow".into()))?,
            )
            .ok_or_else(|| Error::Limit("cost overflow".into()))?;
        if calls > limits.max_calls {
            return Err(Error::Limit(format!("call ceiling {} would be crossed", limits.max_calls)));
        }
        if tokens > limits.max_input_tokens {
            return Err(Error::Limit(format!(
                "input-token ceiling {} would be crossed",
                limits.max_input_tokens
            )));
        }
        if cost > limits.max_cost_nano_usd {
            return Err(Error::Limit(format!(
                "cost ceiling {} nano-USD would be crossed",
                limits.max_cost_nano_usd
            )));
        }
        *self = Self { calls, reserved_input_tokens: tokens, reserved_cost_nano_usd: cost };
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub version: u32,
    #[serde(rename = "runKey")]
    pub run_key: String,
    pub corpus: String,
    pub question: String,
    pub model: String,
    #[serde(rename = "schemaDigest")]
    pub schema_digest: String,
    pub title: TitleKey,
    pub decision: Decision,
    #[serde(rename = "inputTokens")]
    pub input_tokens: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Coverage {
    pub denominator: usize,
    pub decided: usize,
    pub yes: usize,
    pub no: usize,
    pub unknown: usize,
    pub remaining: usize,
    pub complete: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Report {
    #[serde(rename = "runKey")]
    pub run_key: String,
    pub corpus: String,
    pub question: String,
    pub model: String,
    #[serde(rename = "schemaDigest")]
    pub schema_digest: String,
    pub coverage: Coverage,
    pub usage: Usage,
    /// Why a bounded run stopped before its denominator was covered. Absent only when complete.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stopped: Option<String>,
}

impl Report {
    /// MCP-facing shape: coverage always carries its denominator and incompleteness explicitly.
    pub fn as_json(&self) -> Value {
        let mut answer = json!({
            "run_key": self.run_key,
            "corpus": self.corpus,
            "question": self.question,
            "model": self.model,
            "schema_digest": self.schema_digest,
            "coverage": self.coverage,
            "usage": self.usage,
        });
        if let Some(stopped) = &self.stopped {
            answer["stopped"] = json!(stopped);
        }
        answer
    }
}

#[derive(Debug)]
pub enum Error {
    Invalid(String),
    Limit(String),
    Provider(String),
    Io(String),
    Corrupt(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kind, message) = match self {
            Error::Invalid(m) => ("invalid request", m),
            Error::Limit(m) => ("spend refused", m),
            Error::Provider(m) => ("provider failed", m),
            Error::Io(m) => ("persistence failed", m),
            Error::Corrupt(m) => ("persisted decision is invalid", m),
        };
        write!(f, "{kind}: {message}")
    }
}

impl std::error::Error for Error {}

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, Error> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| Error::Io(e.to_string()))?;
        Ok(Self { root })
    }

    fn path(&self, run_key: &str, title: &TitleKey) -> PathBuf {
        self.root.join(run_key).join(format!("{}.json", digest(title.stable().as_bytes())))
    }

    pub fn load(&self, run_key: &str, title: &TitleKey) -> Result<Option<Record>, Error> {
        let path = self.path(run_key, title);
        let mut file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error::Io(format!("{}: {e}", path.display()))),
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(|e| Error::Io(format!("{}: {e}", path.display())))?;
        let record: Record =
            serde_json::from_slice(&bytes).map_err(|e| Error::Corrupt(format!("{}: {e}", path.display())))?;
        if record.version != RECORD_VERSION || record.run_key != run_key || record.title != *title {
            return Err(Error::Corrupt(format!("{} has the wrong identity", path.display())));
        }
        Ok(Some(record))
    }

    pub fn persist(&self, record: &Record) -> Result<(), Error> {
        let path = self.path(&record.run_key, &record.title);
        let parent = path.parent().ok_or_else(|| Error::Io("decision path has no parent".into()))?;
        fs::create_dir_all(parent).map_err(|e| Error::Io(e.to_string()))?;
        if let Some(existing) = self.load(&record.run_key, &record.title)? {
            return if existing == *record {
                Ok(())
            } else {
                Err(Error::Corrupt(format!(
                    "{} conflicts with the decision already persisted",
                    path.display()
                )))
            };
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| Error::Io(e.to_string()))?
            .as_nanos();
        let tmp = path.with_extension(format!("json.tmp-{}-{nonce}", std::process::id()));
        let bytes = serde_json::to_vec(record).map_err(|e| Error::Io(e.to_string()))?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| Error::Io(format!("{}: {e}", tmp.display())))?;
        file.write_all(&bytes).and_then(|_| file.sync_all()).map_err(|e| Error::Io(e.to_string()))?;
        match fs::hard_link(&tmp, &path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = self.load(&record.run_key, &record.title)?;
                if existing.as_ref() != Some(record) {
                    let _ = fs::remove_file(&tmp);
                    return Err(Error::Corrupt(format!(
                        "{} conflicts with a concurrently persisted decision",
                        path.display()
                    )));
                }
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(Error::Io(format!("{}: {e}", path.display())));
            }
        }
        fs::remove_file(&tmp).map_err(|e| Error::Io(format!("{}: {e}", tmp.display())))?;
        sync_dir(parent)?;
        Ok(())
    }
}

fn sync_dir(path: &Path) -> Result<(), Error> {
    fs::File::open(path).and_then(|f| f.sync_all()).map_err(|e| Error::Io(format!("{}: {e}", path.display())))
}

pub fn normalize_question(question: &str) -> Result<String, Error> {
    let normalized = question.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let chars = normalized.chars().count();
    if !(3..=240).contains(&chars) {
        return Err(Error::Invalid("question must be 3 to 240 characters after normalization".into()));
    }
    Ok(normalized)
}

pub fn schema_digest() -> String {
    digest(QUESTION_SCHEMA.as_bytes())
}

pub fn run_key(corpus: &str, question: &str, model: &str) -> Result<String, Error> {
    let question = normalize_question(question)?;
    if corpus.trim().is_empty() || model.trim().is_empty() {
        return Err(Error::Invalid("corpus and model are required".into()));
    }
    let fields = [
        RECORD_VERSION.to_string(),
        NORMALIZATION_VERSION.to_owned(),
        corpus.trim().to_owned(),
        question,
        model.trim().to_owned(),
        schema_digest(),
    ];
    let mut framed = Vec::new();
    for field in fields {
        framed.extend_from_slice(&(field.len() as u64).to_be_bytes());
        framed.extend_from_slice(field.as_bytes());
    }
    Ok(digest(&framed))
}

pub async fn run<P: Provider>(
    store: &Store,
    corpus: &str,
    question: &str,
    candidates: &[Candidate],
    limits: &Limits,
    provider: &P,
) -> Result<Report, Error> {
    if candidates.len() > limits.max_candidates {
        return Err(Error::Limit(format!("candidate ceiling {} would be crossed", limits.max_candidates)));
    }
    let mut unique = HashSet::with_capacity(candidates.len());
    if candidates.iter().any(|candidate| !unique.insert(candidate.title.clone())) {
        return Err(Error::Invalid("candidate population contains a duplicate title".into()));
    }
    let question = normalize_question(question)?;
    let model = provider.model().trim().to_owned();
    let run_key = run_key(corpus, &question, &model)?;
    let schema_digest = schema_digest();
    let mut usage = Usage::default();
    let mut decisions = Vec::with_capacity(candidates.len());
    let mut stopped = None;
    for candidate in candidates {
        if let Some(record) = store.load(&run_key, &candidate.title)? {
            if record.corpus != corpus.trim()
                || record.question != question
                || record.model != model
                || record.schema_digest != schema_digest
            {
                return Err(Error::Corrupt(format!(
                    "persisted {}:{} metadata does not match its run key",
                    candidate.title.media_type, candidate.title.id
                )));
            }
            decisions.push(record.decision);
            continue;
        }
        let estimate = provider.estimate_input_tokens(&question, candidate);
        match usage.reserve(estimate, limits) {
            Ok(()) => {}
            Err(Error::Limit(reason)) => {
                stopped = Some(reason);
                break;
            }
            Err(other) => return Err(other),
        }
        let answer = match provider.decide(&question, QUESTION_SCHEMA, candidate).await {
            Ok(answer) => answer,
            Err(error) => {
                stopped = Some(format!("provider failed: {error}"));
                break;
            }
        };
        if answer.input_tokens > estimate {
            stopped = Some(format!(
                "reported usage {} exceeded the reserved estimate {estimate}",
                answer.input_tokens
            ));
            break;
        }
        let record = Record {
            version: RECORD_VERSION,
            run_key: run_key.clone(),
            corpus: corpus.trim().to_owned(),
            question: question.clone(),
            model: model.clone(),
            schema_digest: schema_digest.clone(),
            title: candidate.title.clone(),
            decision: answer.decision,
            input_tokens: answer.input_tokens,
        };
        store.persist(&record)?;
        decisions.push(answer.decision);
    }
    let yes = decisions.iter().filter(|d| **d == Decision::Yes).count();
    let no = decisions.iter().filter(|d| **d == Decision::No).count();
    let unknown = decisions.iter().filter(|d| **d == Decision::Unknown).count();
    let decided = decisions.len();
    let denominator = candidates.len();
    Ok(Report {
        run_key,
        corpus: corpus.trim().to_owned(),
        question,
        model,
        schema_digest,
        coverage: Coverage {
            denominator,
            decided,
            yes,
            no,
            unknown,
            remaining: denominator.saturating_sub(decided),
            complete: decided == denominator,
        },
        usage,
        stopped,
    })
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct FakeProvider {
        model: String,
        estimate: u64,
        calls: AtomicUsize,
        answers: HashMap<TitleKey, Decision>,
    }

    impl Provider for FakeProvider {
        type Failure = String;

        fn model(&self) -> &str {
            &self.model
        }

        fn estimate_input_tokens(&self, _question: &str, _candidate: &Candidate) -> u64 {
            self.estimate
        }

        fn decide<'a>(
            &'a self,
            _question: &str,
            schema: &str,
            candidate: &'a Candidate,
        ) -> BoxFuture<'a, Result<ProviderAnswer, Self::Failure>> {
            assert_eq!(schema, QUESTION_SCHEMA);
            self.calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                Ok(ProviderAnswer {
                    decision: self.answers.get(&candidate.title).copied().unwrap_or(Decision::Unknown),
                    input_tokens: self.estimate - 1,
                })
            })
        }
    }

    fn temp_store(test: &str) -> (PathBuf, Store) {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("den-mcp-{test}-{}-{nonce}", std::process::id()));
        let store = Store::open(&path).unwrap();
        (path, store)
    }

    fn candidates() -> Vec<Candidate> {
        [
            ("movie", 1, "PRIVATE ARTICLE ONE"),
            ("series", 2, "PRIVATE ARTICLE TWO"),
            ("movie", 3, "PRIVATE ARTICLE THREE"),
        ]
        .into_iter()
        .map(|(kind, id, state)| Candidate { title: TitleKey::new(kind, id).unwrap(), state: state.into() })
        .collect()
    }

    fn provider() -> FakeProvider {
        FakeProvider {
            model: "fake-1".into(),
            estimate: 100,
            calls: AtomicUsize::new(0),
            answers: HashMap::from([
                (TitleKey::new("movie", 1).unwrap(), Decision::Yes),
                (TitleKey::new("series", 2).unwrap(), Decision::No),
            ]),
        }
    }

    fn limits(calls: u64) -> Limits {
        Limits {
            max_candidates: 10,
            max_calls: calls,
            max_input_tokens: calls * 100,
            dollars_per_billion_input_tokens: 42,
            max_cost_nano_usd: calls * 100 * 42,
        }
    }

    #[test]
    fn question_and_key_are_normalized_versioned_and_sensitive() {
        assert_eq!(normalize_question("  Unreliable\n NARRATOR? ").unwrap(), "unreliable narrator?");
        let a = run_key("corpus-a", " Unreliable  Narrator? ", "jev-1").unwrap();
        let b = run_key("corpus-a", "unreliable narrator?", "jev-1").unwrap();
        assert_eq!(a, b);
        assert_ne!(a, run_key("corpus-b", "unreliable narrator?", "jev-1").unwrap());
        assert_ne!(a, run_key("corpus-a", "unreliable narrator?", "jev-2").unwrap());
        assert_eq!(schema_digest().len(), 64);
    }

    #[tokio::test]
    async fn decisions_persist_without_evidence_and_a_second_run_resumes() {
        let (path, store) = temp_store("resume");
        let first = provider();
        let report =
            run(&store, "corpus-a", "Unreliable narrator?", &candidates(), &limits(3), &first).await.unwrap();
        assert_eq!(first.calls.load(Ordering::Relaxed), 3);
        assert_eq!(
            report.coverage,
            Coverage { denominator: 3, decided: 3, yes: 1, no: 1, unknown: 1, remaining: 0, complete: true }
        );
        assert_eq!(report.usage.calls, 3);

        let resumed = provider();
        let resumed_report =
            run(&store, "corpus-a", "unreliable narrator?", &candidates(), &limits(0), &resumed)
                .await
                .unwrap();
        assert_eq!(resumed.calls.load(Ordering::Relaxed), 0);
        assert_eq!(resumed_report.coverage, report.coverage);
        assert_eq!(resumed_report.usage, Usage::default());

        let mut persisted = String::new();
        for entry in fs::read_dir(path.join(&report.run_key)).unwrap() {
            fs::File::open(entry.unwrap().path()).unwrap().read_to_string(&mut persisted).unwrap();
        }
        assert!(!persisted.contains("PRIVATE ARTICLE"), "source evidence must never be persisted");
        fs::remove_dir_all(path).unwrap();
    }

    #[tokio::test]
    async fn every_ceiling_is_reserved_before_the_provider_is_called() {
        for (name, bounded) in [
            ("calls", Limits { max_calls: 1, ..limits(3) }),
            ("tokens", Limits { max_input_tokens: 199, ..limits(3) }),
            ("cost", Limits { max_cost_nano_usd: 8_399, ..limits(3) }),
        ] {
            let (path, store) = temp_store(name);
            let fake = provider();
            let report = run(
                &store,
                &format!("corpus-{name}"),
                "Unreliable narrator?",
                &candidates(),
                &bounded,
                &fake,
            )
            .await
            .unwrap();
            assert!(!report.coverage.complete, "{name}");
            assert!(report.stopped.as_deref().is_some_and(|why| why.contains("ceiling")), "{name}");
            assert!(
                fake.calls.load(Ordering::Relaxed) <= 1,
                "{name}: the refused call must not reach the provider"
            );
            fs::remove_dir_all(path).unwrap();
        }
    }

    #[tokio::test]
    async fn a_larger_budget_resumes_only_the_unfinished_tail() {
        let (path, store) = temp_store("partial");
        let first = provider();
        let partial =
            run(&store, "corpus-a", "Unreliable narrator?", &candidates(), &limits(2), &first).await.unwrap();
        assert_eq!(partial.coverage.decided, 2);
        assert_eq!(partial.coverage.denominator, 3);
        assert_eq!(partial.coverage.remaining, 1);
        assert!(!partial.coverage.complete);
        assert!(partial.stopped.is_some());
        assert_eq!(first.calls.load(Ordering::Relaxed), 2);
        let resumed = provider();
        let report = run(&store, "corpus-a", "Unreliable narrator?", &candidates(), &limits(1), &resumed)
            .await
            .unwrap();
        assert_eq!(resumed.calls.load(Ordering::Relaxed), 1);
        assert!(report.coverage.complete);
        fs::remove_dir_all(path).unwrap();
    }

    #[tokio::test]
    async fn mcp_smoke_fixture_names_denominator_unknowns_and_usage() {
        let (path, store) = temp_store("mcp");
        let fake = provider();
        let report =
            run(&store, "corpus-a", "Unreliable narrator?", &candidates(), &limits(3), &fake).await.unwrap();
        let envelope = crate::mcp::tool_result(Ok(report.as_json()));
        let answer = &envelope["structuredContent"];
        assert_eq!(answer["coverage"]["denominator"], 3);
        assert_eq!(answer["coverage"]["unknown"], 1);
        assert_eq!(answer["coverage"]["complete"], true);
        assert_eq!(answer["usage"]["calls"], 3);
        assert!(answer.get("stopped").is_none());
        assert_eq!(envelope["isError"], Value::Null);
        fs::remove_dir_all(path).unwrap();
    }
}
