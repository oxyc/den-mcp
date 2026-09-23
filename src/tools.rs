//! The tools: what a model may ask Den, and the shape of each answer.
//!
//! # What an answer may carry
//!
//! Allowlisted, field by field, from atlas's answers — never atlas's answer with fields removed, so a field atlas
//! grows tomorrow does not reach a model by default. In particular nothing that comes from TMDB (its overviews,
//! images, ratings, votes and popularity numbers, TMDB's terms forbid their use in an AI application) and no
//! streaming availability. A title is its Den Web link, its name and year, and Den's own labels; people and credits
//! are Wikidata's. Titles come in atlas's popularity order, which is fine; the numbers behind it never appear.
//! Atlas's schema names what of its own answers is TMDB's (`tmdb` in `/index/schema.json`): its filter kinds are
//! refused here, a search hit named only by TMDB is left out, and `guard` refuses any answer that carries prose
//! under the keys atlas's own serving guard refuses.
//!
//! Every title carries its Den Web page (`url`), where the person asking sees posters, ratings and where to watch.
//!
//! # Honesty about the corpus
//!
//! Atlas indexes Den's corpus — tens of thousands of titles, not the catalogue. A list says so, and a title atlas
//! has no card for is `indexed: false`, so a model says "Den has nothing on it" rather than "it does not exist".

use crate::atlas::{encode, Atlas, Failed};
use crate::config::Config;
use crate::names;
use crate::sel::{self, Item, Scope};
use serde_json::{json, Map, Value};

/// Said with every list: what the results are drawn from.
const CORPUS: &str =
    "Den's index of ~47k films and series, not every title; one missing here may still exist.";
/// Where Den's own descriptions come from, said with a title's facts.
const ATTRIBUTION: &str = "Labels and plot facets are derived from Wikipedia article text (CC BY-SA 4.0); \
                           people and credits from Wikidata (CC0).";

pub struct Ctx<'a> {
    pub atlas: &'a Atlas,
    pub cfg: &'a Config,
    pub rid: Option<&'a str>,
    /// This year, for ages.
    pub year: i64,
    /// The filter kinds that are TMDB's (`Atlas::tmdb_kinds`).
    pub tmdb_kinds: &'a [String],
}

/// A tool refused or failed: said to the model as a tool error it can act on.
#[derive(Debug, PartialEq)]
pub struct ToolError(pub String);

impl From<Failed> for ToolError {
    /// Atlas's own words only for a refused question, where they say what to fix. Any other failure is said in
    /// general terms and its detail goes to the log: it can name the network, an address or an internal error.
    fn from(f: Failed) -> Self {
        match f.status {
            Some(400) => ToolError(format!("Den refused the question: {}", clip(&f.detail, 300))),
            Some(404) => ToolError("Den's index does not answer that question.".into()),
            Some(503) => ToolError("Den's index is loading or busy; try again in a few seconds.".into()),
            _ => {
                eprintln!("atlas: {}", f.detail);
                ToolError("Den's index is unavailable right now; try again later.".into())
            }
        }
    }
}

fn clip(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

// ---- the allowlist, value by value
//
// A field copied from atlas is copied as the type it is meant to be, never wholesale: a string field whose value is
// an object, or a list of strings that holds something else, is left out rather than passed through.

/// A short string.
fn text(v: Option<&Value>) -> Option<Value> {
    v?.as_str().map(str::trim).filter(|s| !s.is_empty() && s.chars().count() <= 200).map(|s| json!(s))
}

/// A list of short strings, at most `cap`; `None` when empty.
fn texts(v: Option<&Value>, cap: usize) -> Option<Value> {
    let list: Vec<Value> = v?.as_array()?.iter().filter_map(|x| text(Some(x))).take(cap).collect();
    (!list.is_empty()).then_some(Value::Array(list))
}

fn count(v: Option<&Value>) -> Option<Value> {
    v?.as_u64().map(|n| json!(n))
}

fn integer_value(v: Option<&Value>) -> Option<Value> {
    v?.as_i64().map(|n| json!(n))
}

fn boolean(v: Option<&Value>) -> Option<Value> {
    v?.as_bool().map(|b| json!(b))
}

/// A Wikidata item id: `Q` and digits.
fn qid(v: Option<&Value>) -> Option<Value> {
    let s = v?.as_str()?;
    (s.len() > 1 && s.len() <= 12 && s.starts_with('Q') && s[1..].bytes().all(|b| b.is_ascii_digit()))
        .then(|| json!(s))
}

/// An object of short strings keyed by short lower-case names: plot facets.
fn text_map(v: Option<&Value>) -> Option<Value> {
    let map: Map<String, Value> = v?
        .as_object()?
        .iter()
        .filter(|(k, _)| k.len() <= 32 && k.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'))
        .filter_map(|(k, x)| Some((k.clone(), text(Some(x))?)))
        .collect();
    (!map.is_empty()).then_some(Value::Object(map))
}

fn put(out: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(v) = value {
        out.insert(key.into(), v);
    }
}

type Answer = Result<Value, ToolError>;

fn bad(message: impl Into<String>) -> ToolError {
    ToolError(message.into())
}

/// The tools, as `tools/list` names them.
pub fn list() -> Value {
    let title_ref = json!({
        "type": "object",
        "properties": {
            "type": { "type": "string", "enum": ["movie", "series"] },
            "id": { "type": "integer", "description": "The id a Den result gave" },
        },
        "required": ["type", "id"],
    });
    let sel_items = json!({
        "type": "array", "items": { "type": "string" }, "maxItems": 16,
        "description": "Filters, all must hold, each \"kind:id\" (\"-kind:id\" excludes). Two values of one kind mean \
            both; for either, ask once per value and merge. An example id per kind: language:sv or language:Swedish, \
            country:SE or country:Sweden, region:scandinavian, decade:1990 (no year range: use the decades, or \
            den_search's year_min/year_max), primary:Crime, genre:80 or genre:Crime, subgenre:Heist, \
            mood:Tense/Edge-of-seat, animated:yes, runtime:under-90, source:book, ending:tragic, tone:bleak, \
            pacing:slow-burn, era:medieval, setting:rural, scope:global, chronology:nonlinear, continuity:episodic, \
            conflict:person-vs-self, ensemble:ensemble-led, timespan:single-day, archetype:rebirth, \
            technique:stop_motion, audience:made_for_children, critique:class, warning:graphic_violence, \
            studio:Q16248298 (A24); Wikidata Q-ids for person, made (director/writer/creator), cast, company, \
            network, subject, place, format. Case and separators are forgiven. den_filter_values lists any kind's \
            values and turns names into ids.",
    });
    let type_all = json!({ "type": "string", "enum": ["movie", "series", "all"], "default": "all" });
    let paging = |default: u32, max: u32| {
        (
            json!({ "type": "integer", "minimum": 0, "default": 0, "description": "0-based page" }),
            json!({ "type": "integer", "minimum": 1, "maximum": max, "default": default }),
        )
    };
    let (page, limit20) = paging(20, 100);
    let (_, limit10) = paging(10, 100);
    let read_only = json!({ "readOnlyHint": true, "openWorldHint": false });
    json!([
        {
            "name": "den_search",
            "title": "Search Den",
            "description": "Search Den's film and series index in plain words: a title, a person, \"the one where …\", \
                a mood or theme (\"slow-burn 90s thrillers\", \"bleak korean heist\"). Den reads the words itself; put \
                a year window or language you worked out in the parameters. Returns titles best first, what Den \
                understood, and people the words named; `candidates` is how many the words reached, not a count \
                of matches. For exact criteria use den_filter_titles.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "minLength": 2 },
                    "type": { "type": "string", "enum": ["movie", "series"] },
                    "year_min": { "type": "integer" },
                    "year_max": { "type": "integer" },
                    "language": { "type": "string", "description": "Original language: a name or ISO 639-1" },
                    "runtime_max": { "type": "integer", "description": "Minutes; films only" },
                    "broadcaster": { "type": "string",
                                     "description": "Series only: a network's Q-id (den_filter_values kind=network)" },
                    "page": page,
                    "limit": limit10,
                },
                "required": ["query"],
            },
            "annotations": read_only,
        },
        {
            "name": "den_filter_titles",
            "title": "Filter Den's titles",
            "description": "Titles carrying every filter in `sel`, most popular first, paged, with the total. \
                Languages and countries may be named (language:Swedish); people, companies and places need their \
                Q-id from den_filter_values (\"Christopher Nolan\" → made:Q25191). No OR and no year range: ask \
                once per value and merge, and use decades.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "type": type_all,
                    "sel": sel_items,
                    "order": { "type": "string", "enum": ["popular", "newest", "oldest"], "default": "popular",
                               "description": "Only popular for now; the answer says so for the others" },
                    "page": page,
                    "limit": limit20,
                },
            },
            "annotations": read_only,
        },
        {
            "name": "den_filter_values",
            "title": "Look up filter values",
            "description": "Turn a name into the id a filter takes, with how many titles carry it under `sel`. \
                `kind` is a den_filter_titles kind or a den_find_people trait (gender, citizenship, occupation, born, \
                role). A short list (moods, subgenres, languages, countries, plot facets, …) comes whole; people, \
                companies, places and the like come 10 at most, found by `q`, the start of a word (\"nol\" → \
                Christopher Nolan). `complete: false` means more exist. Without `kind`: each kind's commonest \
                values, or only `kinds`'.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kind": { "type": "string" },
                    "q": { "type": "string" },
                    "kinds": { "type": "array", "items": { "type": "string" }, "description": "Overview only these" },
                    "type": type_all,
                    "sel": sel_items,
                    "limit": { "type": "integer", "minimum": 1, "maximum": 10, "default": 10 },
                },
            },
            "annotations": read_only,
        },
        {
            "name": "den_find_people",
            "title": "Find people",
            "description": "People credited on the titles `sel` matches (e.g. [\"decade:2020\"] for 2020s titles), \
                filtered by what Wikidata states about them. `sort`: prominence (how well known the matching \
                titles they're credited on are; the default), credits (most matching titles), name, youngest, \
                oldest; the answer's note says which order was used, since Den's index may not offer every one. \
                Ages are from birth years, today. Citizenship takes a country's name or code; occupation a Q-id \
                from den_filter_values. People Wikidata has no record for on a trait are left out, so a list is \
                never complete; say so.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "type": type_all,
                    "sel": sel_items,
                    "sort": {
                        "type": "string",
                        "enum": ["prominence", "credits", "name", "youngest", "oldest"],
                        "default": "prominence",
                    },
                    "role": { "type": "string", "enum": ["cast", "director", "writer", "creator"] },
                    "gender": { "type": "string", "description": "male, female, non-binary, or a Wikidata Q-id" },
                    "age_min": { "type": "integer" },
                    "age_max": { "type": "integer" },
                    "born_min": { "type": "integer", "description": "Birth year, instead of an age" },
                    "born_max": { "type": "integer" },
                    "living": { "type": "boolean", "description": "Leave out people who have died" },
                    "citizenship": { "type": "array", "items": { "type": "string" },
                                     "description": "Countries (Sweden, SE) or Q-ids, all held" },
                    "occupation": { "type": "array", "items": { "type": "string" }, "description": "Q-ids, all held" },
                    "page": page,
                    "limit": limit20,
                },
            },
            "annotations": read_only,
        },
        {
            "name": "den_title",
            "title": "A title's facts",
            "description": "One title as Den describes it: Den's genre, subgenres and moods, plot facets (ending, \
                tone, pacing, …), countries, languages, runtime, what it adapts, its iconic studios, and \
                directors/writers (one list) and cast with their Wikidata ids. Techniques, warnings, subjects and \
                places are filters only (den_filter_titles), not listed here. No plot summary, ratings or \
                availability: share its url for those.",
            "inputSchema": title_ref,
            "annotations": read_only,
        },
        {
            "name": "den_similar",
            "title": "More like these",
            "description": "Titles like one or more given titles (ids from other Den tools), best first; several \
                seeds are interleaved. `sel` narrows them with den_filter_titles' filters, e.g. \
                [\"region:scandinavian\"] for \"like Heat but Scandinavian\". Films and series mix unless \
                `mix_types` is false.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "titles": { "type": "array", "items": title_ref, "minItems": 1, "maxItems": 8 },
                    "sel": sel_items,
                    "mix_types": { "type": "boolean", "default": true },
                    "page": page,
                    "limit": limit20,
                },
                "required": ["titles"],
            },
            "annotations": read_only,
        },
    ])
}

/// Run one tool. `None` for a name that is not a tool, which is the protocol's error rather than the tool's.
pub async fn call(ctx: &Ctx<'_>, name: &str, args: &Value) -> Option<Answer> {
    let answer = match name {
        "den_search" => search(ctx, args).await,
        "den_filter_titles" => filter_titles(ctx, args).await,
        "den_filter_values" => filter_values(ctx, args).await,
        "den_find_people" => find_people(ctx, args).await,
        "den_title" => title_facts(ctx, args).await,
        "den_similar" => similar(ctx, args).await,
        _ => return None,
    };
    Some(answer.and_then(guard))
}

/// Keys atlas's serving guard (`tos.rs`) refuses, compared as it compares them: letters and digits, lower-cased.
const PROSE: &[&str] = &[
    "overview",
    "summary",
    "synopsis",
    "description",
    "plot",
    "plotsummary",
    "tagline",
    "storyline",
    "premise",
    "abstract",
    "blurb",
    "logline",
    "review",
];

/// The last boundary before a model: an answer carrying prose under any of those keys is refused whole, whatever
/// built it. The allowlists above never produce one; this is what holds if one of them is wrong.
fn guard(answer: Value) -> Answer {
    fn walk(value: &Value) -> Option<String> {
        match value {
            Value::Object(map) => map.iter().find_map(|(key, child)| {
                let folded: String =
                    key.chars().filter(char::is_ascii_alphanumeric).flat_map(char::to_lowercase).collect();
                if PROSE.contains(&folded.as_str()) {
                    Some(key.clone())
                } else {
                    walk(child)
                }
            }),
            Value::Array(list) => list.iter().find_map(walk),
            _ => None,
        }
    }
    match walk(&answer) {
        None => Ok(answer),
        Some(key) => {
            eprintln!("guard: refused a tool answer carrying {key:?}");
            Err(bad("Den could not answer that safely."))
        }
    }
}

// ---- arguments

fn string<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.trim()).filter(|s| !s.is_empty())),
        Some(_) => Err(bad(format!("{key} must be a string"))),
    }
}

fn integer(args: &Value, key: &str) -> Result<Option<i64>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_i64().map(Some).ok_or_else(|| bad(format!("{key} must be an integer"))),
    }
}

fn strings(args: &Value, key: &str) -> Result<Vec<String>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::String(s)) => Ok(vec![s.clone()]),
        Some(Value::Array(list)) => list
            .iter()
            .map(|v| v.as_str().map(str::to_owned).ok_or_else(|| bad(format!("{key} holds strings"))))
            .collect(),
        Some(_) => Err(bad(format!("{key} must be a list of strings"))),
    }
}

fn scope(args: &Value) -> Result<Scope, ToolError> {
    match string(args, "type")? {
        None => Ok(Scope::All),
        Some(t) => Scope::parse(t).ok_or_else(|| bad("type is movie, series or all")),
    }
}

/// The page and its size, as atlas takes them.
fn paging(args: &Value, default: usize) -> Result<(usize, usize), ToolError> {
    let page = integer(args, "page")?.unwrap_or(0);
    let limit = integer(args, "limit")?.unwrap_or(default as i64);
    if page < 0 || limit < 1 {
        return Err(bad("page is 0 or more and limit 1 or more"));
    }
    Ok((page as usize, (limit as usize).min(sel::MAX_PAGE)))
}

/// A title selection, and how any value in it was read (`read_as`). Refused: the kinds that are TMDB's (ratings,
/// characters), `like`, which is den_similar's, and the person traits, which are den_find_people's.
async fn selection(ctx: &Ctx<'_>, args: &Value, scope: Scope) -> Result<(Vec<Item>, Vec<String>), ToolError> {
    let raw = strings(args, "sel")?;
    refuse_tmdb_kinds(&raw, ctx.tmdb_kinds)?;
    let (raw, read_as) = vocabulary(ctx, raw).await?;
    let items = sel::items(&raw, scope).map_err(bad)?;
    for item in &items {
        let kind = item.kind.as_str();
        if kind == "like" {
            return Err(bad("use den_similar for titles like another"));
        }
        if sel::TRAITS.contains(&kind) {
            return Err(bad(format!("{kind} is a person trait: pass it to den_find_people")));
        }
    }
    Ok((items, read_as))
}

/// A TMDB-backed kind is refused as such before anything else reads it, the ones only atlas's schema names too.
fn refuse_tmdb_kinds(raw: &[String], tmdb_kinds: &[String]) -> Result<(), ToolError> {
    for item in raw.iter().flat_map(|i| i.split(',')) {
        let kind = item.trim().trim_start_matches('-').split(':').next().unwrap_or("").trim().to_lowercase();
        if tmdb_kinds.contains(&kind) {
            return Err(bad(format!(
                "{kind} is not offered here; a title's url shows what Den Web has on it"
            )));
        }
    }
    Ok(())
}

/// The kinds whose values are a short list Den spells exactly: a value is read against the list, whatever its case
/// or separators, and an unknown one is refused with the nearest.
const SPELLED: &[&str] = &[
    "mood",
    "subgenre",
    "primary",
    "region",
    "animated",
    "runtime",
    "source",
    "technique",
    "audience",
    "critique",
    "warning",
    "era",
    "setting",
    "scope",
    "ending",
    "pacing",
    "chronology",
    "continuity",
    "conflict",
    "ensemble",
    "tone",
    "timespan",
    "archetype",
];

/// Den's own vocabulary: every value of every short-list kind, with its label where it has one. counts.json with no
/// selection lists them whole; atlas marks it fresh for an hour, and it is kept here for that long.
async fn vocabulary_of(ctx: &Ctx<'_>) -> Option<Value> {
    ctx.atlas.get(&sel::counts_url(Scope::All, &[]), ctx.rid).await.ok().map(|(v, _)| v)
}

/// `sel` read against Den's vocabulary: "mood:tense edge of seat" is `mood:Tense/Edge-of-seat`, "genre:crime" is
/// `genre:80`. What was read differently is said back, and a value Den does not have is refused with the nearest.
async fn vocabulary(ctx: &Ctx<'_>, raw: Vec<String>) -> Result<(Vec<String>, Vec<String>), ToolError> {
    let mut vocab: Option<Option<Value>> = None;
    let mut out = Vec::with_capacity(raw.len());
    let mut read_as = Vec::new();
    for item in raw.iter().flat_map(|i| i.split(',')).map(str::trim).filter(|i| !i.is_empty()) {
        let (exclude, rest) = match item.strip_prefix('-') {
            Some(rest) => ("-", rest),
            None => ("", item),
        };
        let Some((kind, id)) = rest.split_once(':') else {
            out.push(item.to_owned());
            continue;
        };
        let (kind, id) = (kind.trim().to_ascii_lowercase(), id.trim());
        let known: Vec<(String, Option<String>)> = if kind == "genre" {
            if id.parse::<u32>().is_ok() {
                out.push(item.to_owned());
                continue;
            }
            GENRES.iter().map(|&(n, name)| (n.to_string(), Some(name.to_owned()))).collect()
        } else if SPELLED.contains(&kind.as_str()) {
            if vocab.is_none() {
                vocab = Some(vocabulary_of(ctx).await);
            }
            let about =
                vocab.as_ref().and_then(Option::as_ref).and_then(|v| v.pointer(&format!("/kinds/{kind}")));
            let labels = about.and_then(|a| a.get("labels"));
            match about.and_then(|a| a.get("values")).and_then(Value::as_object) {
                Some(values) => values
                    .keys()
                    .map(|k| {
                        (k.clone(), labels.and_then(|l| l.get(k)).and_then(Value::as_str).map(str::to_owned))
                    })
                    .collect(),
                // Atlas did not list the kind (or could not be asked): the value goes as given, and atlas says
                // whether it knows it.
                None => {
                    out.push(item.to_owned());
                    continue;
                }
            }
        } else {
            out.push(item.to_owned());
            continue;
        };
        if known.iter().any(|(k, _)| k == id) {
            out.push(item.to_owned());
            continue;
        }
        let key = sel::name_key(id);
        let matched: Vec<&String> = known
            .iter()
            .filter(|(k, label)| {
                sel::name_key(k) == key || label.as_deref().is_some_and(|l| sel::name_key(l) == key)
            })
            .map(|(k, _)| k)
            .collect();
        if let [one] = matched[..] {
            read_as.push(format!("{kind}:{id} as {kind}:{one}"));
            out.push(format!("{exclude}{kind}:{one}"));
            continue;
        }
        let nearest = nearest(&key, &known);
        return Err(bad(format!(
            "{kind}:{id:?} is not one of Den's {kind} values. Nearest: {}. den_filter_values kind={kind} lists them.",
            nearest.join(", ")
        )));
    }
    Ok((out, read_as))
}

/// The three values nearest a folded `key`: containing it or contained in it first, then by edit distance.
fn nearest(key: &str, known: &[(String, Option<String>)]) -> Vec<String> {
    let mut scored: Vec<(usize, &String)> = known
        .iter()
        .map(|(k, label)| {
            let name = sel::name_key(label.as_deref().unwrap_or(k));
            let folded = sel::name_key(k);
            let score =
                if !key.is_empty() && (folded.contains(key) || key.contains(&folded) || name.contains(key)) {
                    0
                } else {
                    1 + edit_distance(key, &folded).min(edit_distance(key, &name))
                };
            (score, k)
        })
        .collect();
    scored.sort();
    scored.into_iter().take(3).map(|(_, k)| k.clone()).collect()
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let here = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != *cb)).min(row[j] + 1).min(here + 1);
            prev = here;
        }
    }
    row[b.len()]
}

// ---- what an answer is made of

/// A title's name as Den Web writes it in a link (`route.ts` `slug`): lowercase words and hyphens, at most ~60.
pub fn slug(name: &str) -> String {
    let folded: String = name.to_lowercase().chars().map(fold).collect();
    let mut words = String::with_capacity(folded.len());
    for c in folded.chars() {
        if c.is_ascii_alphanumeric() {
            words.push(c);
        } else if !words.ends_with('-') {
            words.push('-');
        }
    }
    let words = words.trim_matches('-');
    if words.len() <= 60 {
        return words.to_owned();
    }
    let cut = &words[..60];
    cut.rfind('-').map_or(cut, |i| &cut[..i]).trim_end_matches('-').to_owned()
}

fn fold(c: char) -> char {
    const FROM: &str = "àáâãäåāăąçćčďèéêëēėęěìíîïīįñńňòóôõöøōőŕřśšşťùúûüūůűųýÿźżžðł";
    const TO: &str = "aaaaaaaaacccdeeeeeeeeiiiiiinnnooooooooorrssstuuuuuuuuyyzzzdl";
    FROM.chars().position(|f| f == c).and_then(|i| TO.chars().nth(i)).unwrap_or(c)
}

fn title_url(cfg: &Config, kind: &str, id: u64, name: Option<&str>) -> String {
    let path = if kind == "series" { "tv" } else { "movie" };
    match name.map(slug).filter(|s| !s.is_empty()) {
        Some(s) => format!("{}/{path}/{id}-{s}", cfg.web_origin),
        None => format!("{}/{path}/{id}", cfg.web_origin),
    }
}

/// A person's Den Web page, which is keyed by their TMDB person id: that id is used here and never shown.
fn person_url(cfg: &Config, tmdb: Option<u64>, name: Option<&str>) -> Option<String> {
    let id = tmdb?;
    Some(match name.map(slug).filter(|s| !s.is_empty()) {
        Some(s) => format!("{}/person/{id}-{s}", cfg.web_origin),
        None => format!("{}/person/{id}", cfg.web_origin),
    })
}

/// A card as a result: its type, id, name, year, Den's primary genre, original language and Den Web link.
fn title(cfg: &Config, card: &Value) -> Option<Value> {
    let kind = card.get("type")?.as_str().filter(|t| matches!(*t, "movie" | "series"))?;
    let id = card.get("id")?.as_u64()?;
    let name = card.get("title").and_then(Value::as_str);
    let mut out = Map::new();
    out.insert("type".into(), json!(kind));
    out.insert("id".into(), json!(id));
    put(&mut out, "title", text(card.get("title")));
    put(&mut out, "year", integer_value(card.get("year")));
    put(&mut out, "genre", text(card.get("primaryGenre")));
    put(&mut out, "language", text(card.get("originalLanguage")));
    out.insert("url".into(), json!(title_url(cfg, kind, id, name)));
    Some(Value::Object(out))
}

fn titles(cfg: &Config, cards: Option<&Value>) -> Vec<Value> {
    cards.and_then(Value::as_array).into_iter().flatten().filter_map(|c| title(cfg, c)).collect()
}

/// A list's envelope: the results, the total, whether there is more, and what the results are drawn from.
fn listing(results: Vec<Value>, total: Option<u64>, page: usize, limit: usize) -> Map<String, Value> {
    let mut out = Map::new();
    let shown = (page * limit + results.len()) as u64;
    if let Some(total) = total {
        out.insert("total".into(), json!(total));
        out.insert("more".into(), json!(total > shown));
    }
    out.insert("page".into(), json!(page));
    out.insert("results".into(), Value::Array(results));
    out.insert("corpus".into(), json!(CORPUS));
    out
}

/// What a count is out of, as a model should say it: "12 of 47,613 indexed titles".
fn out_of(out: &mut Map<String, Value>, denominator: Option<&Value>) {
    if let Some(n) = denominator.and_then(Value::as_u64) {
        out.insert("out_of".into(), json!(format!("{} indexed titles", thousands(n))));
    }
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// What atlas said it ignored or did not recognise, which a model should correct rather than silently lose.
fn caveats(ctx: &Ctx<'_>, out: &mut Map<String, Value>, answer: &Value) {
    for (field, key) in [
        ("ignored", "ignored"),
        ("unknownValues", "unknown_values"),
        ("kindsUnavailable", "kinds_unavailable"),
        ("ignoredTraits", "ignored_traits"),
        ("unknownTraits", "unknown_traits"),
        ("traitsUnavailable", "traits_unavailable"),
    ] {
        // A TMDB kind this tool never offers is nothing to tell a model about, however it is spelled there.
        let kept: Vec<Value> = texts(answer.get(field), 16)
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter(|v| {
                let named = v.as_str().unwrap_or("").trim_start_matches('-');
                let kind = named.split(':').next().unwrap_or(named);
                !ctx.tmdb_kinds.iter().any(|k| k == kind)
            })
            .collect();
        if !kept.is_empty() {
            out.insert(key.into(), Value::Array(kept));
        }
    }
}

// ---- den_search

async fn search(ctx: &Ctx<'_>, args: &Value) -> Answer {
    let query = string(args, "query")?
        .filter(|q| q.chars().count() >= 2)
        .ok_or_else(|| bad("query: at least 2 characters"))?;
    let (page, limit) = paging(args, 10)?;
    let mut url = format!("/index/query.json?q={}", encode(query));
    if let Some(t) = string(args, "type")? {
        if !matches!(t, "movie" | "series") {
            return Err(bad("type is movie or series"));
        }
        url.push_str(&format!("&type={t}"));
    }
    for key in ["year_min", "year_max", "runtime_max"] {
        if let Some(n) = integer(args, key)? {
            url.push_str(&format!("&{key}={n}"));
        }
    }
    if let Some(language) = string(args, "language")? {
        let code = names::language(language)
            .ok_or_else(|| bad("language is a language's name or ISO 639-1 code, e.g. Swedish or sv"))?;
        url.push_str(&format!("&language={code}"));
    }
    // A series' broadcaster, as the network kind's Q-id.
    if let Some(broadcaster) = string(args, "broadcaster")? {
        let (_, qid) = sel::normalise("network", broadcaster, Scope::All)
            .map_err(|_| bad("broadcaster is a network's Q-id from den_filter_values kind=network"))?;
        url.push_str(&format!("&broadcaster={qid}"));
    }
    url.push_str(&format!("&skip={}&limit={limit}", page * limit));
    let (answer, _) = ctx.atlas.get(&url, ctx.rid).await?;
    // A hit the corpus has no card for is named by TMDB's export (`titleFrom: "tmdb"`): its name is TMDB's, so it
    // is left out, and the answer says how many were.
    let hits = answer.get("hits").and_then(Value::as_array).cloned().unwrap_or_default();
    let (named, tmdb): (Vec<Value>, Vec<Value>) =
        hits.into_iter().partition(|h| h.get("titleFrom").and_then(Value::as_str) != Some("tmdb"));
    let results = titles(ctx.cfg, Some(&Value::Array(named)));
    let shown = (page * limit + results.len()) as u64;
    let mut out = listing(results, None, page, limit);
    // Search ranks candidates rather than filtering to a set, so its count is how many titles the words reached, not
    // how many match: said as such, never as a total.
    if let Some(candidates) = answer.get("total").and_then(Value::as_u64) {
        out.insert("candidates".into(), json!(candidates));
        out.insert("more".into(), json!(candidates > shown));
        out.insert(
            "candidates_note".into(),
            json!("How many titles the words reached, best first; not a count of exact matches. For an exact \
                   count, den_filter_titles."),
        );
    }
    if !tmdb.is_empty() {
        out.insert(
            "left_out".into(),
            json!(format!("{} matches Den has no description of, only a TMDB name", tmdb.len())),
        );
    }
    let mut not_applied: Vec<Value> = Vec::new();
    for key in ["ignored", "unknownValues"] {
        if let Some(Value::Array(list)) = texts(answer.get(key), 16) {
            not_applied.extend(list);
        }
    }
    if !not_applied.is_empty() {
        out.insert("not_applied".into(), Value::Array(not_applied));
        out.insert("note".into(), json!("Part of the question was not applied: see not_applied."));
    }
    let mut understood = Map::new();
    if let Some(parse) = answer.get("parse") {
        put(&mut understood, "type", text(parse.get("mediaType")));
        put(&mut understood, "country", text(parse.get("country")));
        put(&mut understood, "decade", integer_value(parse.get("decade")));
        put(&mut understood, "year_min", integer_value(parse.get("yearMin")));
        put(&mut understood, "year_max", integer_value(parse.get("yearMax")));
        put(&mut understood, "language", text(parse.get("language")));
        put(&mut understood, "runtime_max", count(parse.get("runtimeMax")));
        put(&mut understood, "labels", texts(parse.get("labels"), 16));
        put(&mut understood, "plot_facets", texts(parse.get("plotFacets"), 16));
        put(&mut understood, "based_on", texts(parse.get("basedOnKind"), 16));
        put(&mut understood, "theme", text(parse.get("leftover")));
        let genres: Vec<Value> = parse
            .get("genres")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_u64)
            .filter_map(genre_name)
            .map(|g| json!(g))
            .collect();
        if !genres.is_empty() {
            understood.insert("genres".into(), Value::Array(genres));
        }
        put(&mut understood, "excluded", texts(answer.pointer("/parse/excluded/phrases"), 16));
        // Words read as something to leave out ("without gore") that matched nothing Den can leave out: nothing
        // was, and a model should not say otherwise.
        if let Some(excluded) = parse.get("excluded").filter(|e| e.is_object()) {
            let listed = |key: &str| excluded.get(key).and_then(Value::as_array).is_some_and(|l| !l.is_empty());
            let kinds =
                ["countries", "decades", "mediaTypes", "genres", "labels", "plotFacets", "basedOnKind", "people"];
            let matched = kinds.iter().any(|k| listed(k))
                || excluded.get("titles").and_then(Value::as_u64).unwrap_or(0) > 0;
            if !matched && listed("phrases") {
                out.insert(
                    "exclusion_note".into(),
                    json!("Den read part of the question as something to leave out but matched it to nothing, so \
                           nothing was left out. For content, den_filter_titles with -warning:<id> (den_filter_values \
                           kind=warning lists them), or another -kind:<id>."),
                );
            }
        }
    }
    if !understood.is_empty() {
        out.insert("understood".into(), Value::Object(understood));
    }
    let people: Vec<Value> = answer
        .get("people")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let name = p.get("name").and_then(Value::as_str);
            let mut person = Map::new();
            person.insert("id".into(), qid(p.get("qid"))?);
            put(&mut person, "name", text(p.get("name")));
            put(&mut person, "credits", count(p.get("credits")));
            put(
                &mut person,
                "url",
                person_url(ctx.cfg, p.get("id").and_then(Value::as_u64), name).map(|u| json!(u)),
            );
            Some(Value::Object(person))
        })
        .collect();
    if !people.is_empty() {
        out.insert("people".into(), Value::Array(people));
    }
    Ok(Value::Object(out))
}

/// TMDB's genre names, for the ids Den's `genre` kind and search's reading of words are written in.
const GENRES: &[(u64, &str)] = &[
    (28, "Action"),
    (12, "Adventure"),
    (16, "Animation"),
    (35, "Comedy"),
    (80, "Crime"),
    (99, "Documentary"),
    (18, "Drama"),
    (10751, "Family"),
    (14, "Fantasy"),
    (36, "History"),
    (27, "Horror"),
    (10402, "Music"),
    (9648, "Mystery"),
    (10749, "Romance"),
    (878, "Science Fiction"),
    (10770, "TV Movie"),
    (53, "Thriller"),
    (10752, "War"),
    (37, "Western"),
    (10759, "Action & Adventure"),
    (10762, "Kids"),
    (10763, "News"),
    (10764, "Reality"),
    (10765, "Sci-Fi & Fantasy"),
    (10766, "Soap"),
    (10767, "Talk"),
    (10768, "War & Politics"),
];

pub fn genre_name(id: u64) -> Option<&'static str> {
    GENRES.iter().find(|(n, _)| *n == id).map(|(_, name)| *name)
}

// ---- den_filter_titles

async fn filter_titles(ctx: &Ctx<'_>, args: &Value) -> Answer {
    let scope = scope(args)?;
    let order = string(args, "order")?;
    if !matches!(order, None | Some("popular" | "newest" | "oldest")) {
        return Err(bad("order is popular, newest or oldest"));
    }
    let (sel, read_as) = selection(ctx, args, scope).await?;
    let (page, limit) = paging(args, 20)?;
    let (skip, limit) = sel::page(page, limit);
    let (answer, _) = ctx.atlas.get(&sel::titles_url(scope, &sel, skip, limit), ctx.rid).await?;
    let results = titles(ctx.cfg, answer.get("titles"));
    let mut out = listing(results, answer.get("total").and_then(Value::as_u64), page, limit);
    out_of(&mut out, answer.get("denominator"));
    // How much of the corpus each applied kind is on record for: a filter on a kind known for a quarter of the
    // titles can only ever find within that quarter.
    let of = match scope {
        Scope::Movie => "films",
        Scope::Series => "series",
        Scope::All => "titles",
    };
    if let Some(coverage) = answer.get("coverage").and_then(Value::as_object) {
        let shares: Map<String, Value> = coverage
            .iter()
            .filter(|(kind, _)| !ctx.tmdb_kinds.contains(*kind))
            .filter_map(|(kind, c)| {
                let (n, total) = (c.get("count")?.as_f64()?, c.get("denominator")?.as_f64()?);
                (total > 0.0).then(|| (kind.clone(), json!(format!("{:.0}% of {of}", n / total * 100.0))))
            })
            .collect();
        if !shares.is_empty() {
            out.insert("on_record".into(), Value::Object(shares));
        }
    }
    // Titles come most popular first. Atlas has no other order for them yet, so another is said, not faked.
    if let Some(order @ ("newest" | "oldest")) = order {
        out.insert(
            "order_note".into(),
            json!(format!(
                "Den's index can't order titles by {order} yet; these are most popular first. Narrow by \
                 decade:<year> instead, or den_search with year_min/year_max."
            )),
        );
    }
    said(&mut out, read_as);
    caveats(ctx, &mut out, &answer);
    Ok(Value::Object(out))
}

/// How values in the question were read, when not as given: "mood:tense edge of seat as mood:Tense/Edge-of-seat".
fn said(out: &mut Map<String, Value>, read_as: Vec<String>) {
    if !read_as.is_empty() {
        out.insert("read_as".into(), json!(read_as));
    }
}

// ---- den_filter_values

/// How many of a kind's commonest values the overview lists.
const OVERVIEW_TOP: usize = 8;

async fn filter_values(ctx: &Ctx<'_>, args: &Value) -> Answer {
    let scope = scope(args)?;
    let (sel, read_as) = selection(ctx, args, scope).await?;
    let limit = integer(args, "limit")?.unwrap_or(10).clamp(1, 10) as usize;
    let q = string(args, "q")?;
    let Some(kind) = string(args, "kind")?.map(str::to_ascii_lowercase) else {
        let only: Vec<String> =
            strings(args, "kinds")?.iter().map(|k| k.trim().to_ascii_lowercase()).collect();
        return overview(ctx, scope, &sel, &only, read_as).await;
    };
    // Only a kind from the known list reaches a URL: it is a path segment there (`sel::values_url`).
    if ctx.tmdb_kinds.contains(&kind) {
        return Err(bad(format!("{kind} is not offered here")));
    }
    if kind == "like" {
        return Err(bad("use den_similar for titles like another"));
    }
    if sel::TRAITS.contains(&kind.as_str()) {
        return trait_values(ctx, scope, &sel, &kind, q, limit).await;
    }
    if !sel::title_kind(&kind) {
        return Err(bad("kind is one of den_filter_titles' kinds, or a person trait"));
    }
    // A kind atlas lists whole comes back whole, named, and matched by q here.
    let (counts, _) = ctx.atlas.get(&sel::counts_url(scope, &sel), ctx.rid).await?;
    if let Some(about) =
        counts.pointer(&format!("/kinds/{kind}")).filter(|a| a.get("complete") == Some(&json!(true)))
    {
        let mut out = whole_list(&kind, about, q);
        out_of(&mut out, counts.get("denominator"));
        said(&mut out, read_as);
        caveats(ctx, &mut out, &counts);
        return Ok(Value::Object(out));
    }
    if let Some(q) = q.filter(|q| sel::name_key(q).chars().count() < 2) {
        return Err(bad(format!("q {q:?}: at least 2 letters")));
    }
    let Some(url) = sel::values_url(scope, &kind, &sel, q, limit) else {
        return Err(bad("kind is one of den_filter_titles' kinds, or a person trait"));
    };
    let (answer, _) = ctx.atlas.get(&url, ctx.rid).await?;
    let people = matches!(kind.as_str(), "person" | "made" | "cast");
    let values: Vec<Value> = answer
        .get("values")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| {
            let id = text(v.get("id"))?;
            let name = text(v.get("name")).unwrap_or_else(|| id.clone());
            let mut value = json!({ "id": id, "name": name });
            let map = value.as_object_mut()?;
            put(map, "count", count(v.get("count")));
            // A person's Den Web page, from the TMDB id atlas names them by: the id itself is never shown.
            if people {
                let url = person_url(ctx.cfg, v.get("tmdbId").and_then(Value::as_u64), name.as_str());
                put(map, "url", url.map(|u| json!(u)));
            }
            Some(value)
        })
        .collect();
    let mut out = Map::new();
    out.insert("kind".into(), json!(kind));
    out.insert("use_as".into(), json!(format!("\"{kind}:<id>\" in sel")));
    out.insert("values".into(), Value::Array(values));
    put(&mut out, "complete", boolean(answer.get("complete")));
    out_of(&mut out, answer.get("denominator"));
    said(&mut out, read_as);
    caveats(ctx, &mut out, &answer);
    Ok(Value::Object(out))
}

/// A value's name as a model should read it: a language's or country's English name, a genre's, else atlas's label.
fn value_name(kind: &str, id: &str, labels: Option<&Value>) -> Option<String> {
    match kind {
        "language" => names::language_name(id).map(str::to_owned),
        "country" => names::country_name(id).map(str::to_owned),
        "genre" => id.parse().ok().and_then(genre_name).map(str::to_owned),
        _ => labels.and_then(|l| l.get(id)).and_then(Value::as_str).map(str::to_owned),
    }
}

/// A short-list kind whole, from counts.json: every value with its count under the selection, most titles first,
/// matched by `q` on its name, its id, or (for a language or country) any other name for it.
fn whole_list(kind: &str, about: &Value, q: Option<&str>) -> Map<String, Value> {
    let labels = about.get("labels");
    let q_key = q.map(sel::name_key).filter(|q| !q.is_empty());
    let named_by_q: Vec<&str> = match (kind, q) {
        ("language", Some(q)) => names::languages_matching(q),
        ("country", Some(q)) => names::countries_matching(q),
        _ => Vec::new(),
    };
    let starts = |text: &str, q: &str| {
        let key = sel::name_key(text);
        key.starts_with(q) || key.split(' ').any(|w| w.starts_with(q))
    };
    let mut values: Vec<(String, Option<String>, u64)> = about
        .get("values")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(id, n)| Some((id.clone(), value_name(kind, id, labels), n.as_u64()?)))
        .filter(|(id, name, _)| {
            q_key.as_deref().is_none_or(|q| {
                named_by_q.contains(&id.as_str())
                    || starts(id, q)
                    || name.as_deref().is_some_and(|n| starts(n, q))
            })
        })
        .collect();
    values.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
    let listed: Vec<Value> = values
        .into_iter()
        .map(|(id, name, n)| match name.filter(|name| *name != id) {
            Some(name) => json!({ "id": id, "name": name, "count": n }),
            None => json!({ "id": id, "count": n }),
        })
        .collect();
    let mut out = Map::new();
    out.insert("kind".into(), json!(kind));
    out.insert("use_as".into(), json!(format!("\"{kind}:<id>\" in sel")));
    if listed.is_empty() {
        if let Some(q) = q {
            out.insert(
                "note".into(),
                json!(format!(
                    "No {kind} under this sel matches {q:?}; without q the whole list comes back."
                )),
            );
        }
    }
    out.insert("values".into(), Value::Array(listed));
    out.insert("complete".into(), json!(true));
    out
}

/// Every kind's commonest values under the selection, named; only the kinds in `only` when it names any.
async fn overview(
    ctx: &Ctx<'_>,
    scope: Scope,
    sel: &[Item],
    only: &[String],
    read_as: Vec<String>,
) -> Answer {
    let (answer, _) = ctx.atlas.get(&sel::counts_url(scope, sel), ctx.rid).await?;
    let mut kinds = Map::new();
    for (kind, about) in answer.get("kinds").and_then(Value::as_object).into_iter().flatten() {
        if ctx.tmdb_kinds.contains(kind) || (!only.is_empty() && !only.contains(kind)) {
            continue;
        }
        let labels = about.get("labels");
        let mut values: Vec<(String, u64)> = about
            .get("values")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter_map(|(id, n)| Some((id.clone(), n.as_u64()?)))
            .collect();
        values.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let listed: Vec<Value> = values
            .into_iter()
            .take(OVERVIEW_TOP)
            .map(|(id, n)| match value_name(kind, &id, labels).filter(|name| *name != id) {
                Some(name) => json!({ "id": id, "name": name, "count": n }),
                None => json!({ "id": id, "count": n }),
            })
            .collect();
        if !listed.is_empty() {
            kinds.insert(kind.clone(), Value::Array(listed));
        }
    }
    let mut out = Map::new();
    out.insert("total".into(), answer.get("total").cloned().unwrap_or(Value::Null));
    out_of(&mut out, answer.get("denominator"));
    out.insert("kinds".into(), Value::Object(kinds));
    out.insert("corpus".into(), json!(CORPUS));
    said(&mut out, read_as);
    caveats(ctx, &mut out, &answer);
    Ok(Value::Object(out))
}

/// A person trait's values under the selection, named and matched by `q`: from atlas's own lookup where it has one
/// (`people/values/<trait>.json`, den-atlas#80), which counts and searches every value, else from
/// `people/counts.json`, which lists the commonest. `complete` is atlas's word, never claimed past it.
async fn trait_values(
    ctx: &Ctx<'_>,
    scope: Scope,
    sel: &[Item],
    kind: &str,
    q: Option<&str>,
    limit: usize,
) -> Answer {
    // A country named in q is its citizenship item from Wikidata's table, whether or not atlas lists it among the
    // commonest.
    let named = (kind == "citizenship")
        .then(|| q.and_then(names::country))
        .flatten()
        .and_then(|code| Some((names::country_item(code)?, names::country_name(code)?)));
    let mut out = Map::new();
    out.insert("kind".into(), json!(kind));
    out.insert("use_as".into(), json!(format!("den_find_people's {kind}")));
    let lookup =
        sel::people_values_url(scope, sel, kind, q.filter(|q| sel::name_key(q).chars().count() >= 2), limit);
    let from_route = match lookup {
        Some(url) => match ctx.atlas.get(&url, ctx.rid).await {
            Ok((answer, _)) => Some(answer),
            // An atlas without the route: the counts below.
            Err(crate::atlas::Failed { status: Some(404), .. }) => None,
            Err(e) => return Err(e.into()),
        },
        None => None,
    };
    let (mut listed, complete, answer) = match from_route {
        Some(answer) => {
            let listed: Vec<Value> = answer
                .get("values")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|v| {
                    let id = text(v.get("id"))?;
                    let name = text(v.get("name")).unwrap_or_else(|| id.clone());
                    Some(json!({ "id": id, "name": name, "people": count(v.get("count")) }))
                })
                .collect();
            let complete = answer.get("complete") == Some(&json!(true));
            (listed, complete, answer)
        }
        None => {
            let (answer, _) =
                ctx.atlas.get(&sel::people_url(scope, sel, &[], sel::ORDERS[0], None), ctx.rid).await?;
            let about = answer.pointer(&format!("/traits/{kind}"));
            let labels = about.and_then(|a| a.get("labels"));
            let q = q.map(sel::name_key).filter(|q| !q.is_empty());
            let mut values: Vec<(String, String, u64)> = about
                .and_then(|a| a.get("values"))
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .filter_map(|(id, n)| {
                    let name =
                        labels.and_then(|l| l.get(id)).and_then(Value::as_str).unwrap_or(id).to_owned();
                    Some((id.clone(), name, n.as_u64()?))
                })
                .filter(|(id, name, _)| {
                    q.as_deref().is_none_or(|q| {
                        let key = sel::name_key(name);
                        key.starts_with(q)
                            || key.split(' ').any(|w| w.starts_with(q))
                            || id.eq_ignore_ascii_case(q)
                    })
                })
                .collect();
            values.sort_by(|a, b| b.2.cmp(&a.2).then(a.1.cmp(&b.1)));
            let shown_all = values.len() <= limit;
            // people/counts.json lists a trait's commonest values; it says whether that is all of them.
            let complete = shown_all && about.and_then(|a| a.get("complete")) == Some(&json!(true));
            let listed = values
                .into_iter()
                .take(limit)
                .map(|(id, name, n)| json!({ "id": id, "name": name, "people": n }))
                .collect();
            if !complete {
                out.insert(
                    "note".into(),
                    json!("Only the commonest values are listed; one missing here may still match people."),
                );
            }
            (listed, complete, answer)
        }
    };
    if let Some((item, name)) = named {
        if !listed.iter().any(|v| v["id"] == item) {
            listed.insert(0, json!({ "id": item, "name": name }));
        }
    }
    out.insert("values".into(), Value::Array(listed));
    out.insert("complete".into(), json!(complete));
    caveats(ctx, &mut out, &answer);
    Ok(Value::Object(out))
}

// ---- den_find_people

/// The most decades an age range may span: each is one request to atlas.
const MAX_DECADES: i64 = 8;

/// Wikidata's items for the genders a person is most often asked about by name.
fn gender_id(given: &str) -> Result<String, ToolError> {
    Ok(match given.to_ascii_lowercase().as_str() {
        "male" | "man" | "men" => "Q6581097".into(),
        "female" | "woman" | "women" => "Q6581072".into(),
        "non-binary" | "nonbinary" => "Q48270".into(),
        other => sel::normalise("gender", other, Scope::All).map(|(_, id)| id).map_err(|_| {
            bad("gender is male, female, non-binary, or a Q-id from den_filter_values kind=gender")
        })?,
    })
}

/// A birth or death as Wikidata dates it, as a model reads it: a date, a year, a decade or a century.
fn date(value: &Value) -> Option<Value> {
    Some(match value.get("precision")?.as_str()? {
        "day" | "month" => value.get("date")?.clone(),
        "year" => value.get("year")?.clone(),
        "decade" => json!(format!("{}s", value.get("year")?.as_i64()?)),
        "century" => json!(format!("{} century", ordinal(value.get("century")?.as_i64()?))),
        _ => return None,
    })
}

fn ordinal(n: i64) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// A birth's year when it is known to the year or better.
fn exact_year(value: &Value) -> Option<i64> {
    matches!(value.get("precision")?.as_str()?, "day" | "month" | "year")
        .then(|| value.get("year")?.as_i64())?
}

async fn find_people(ctx: &Ctx<'_>, args: &Value) -> Answer {
    let scope = scope(args)?;
    let (sel, mut read_as) = selection(ctx, args, scope).await?;
    let (page, limit) = paging(args, 20)?;
    let mut traits: Vec<String> = Vec::new();
    if let Some(role) = string(args, "role")? {
        traits.push(format!("role:{role}"));
    }
    if let Some(gender) = string(args, "gender")? {
        traits.push(format!("gender:{}", gender_id(gender)?));
    }
    // A citizenship is a Wikidata item, or a country as people name it: "Sweden", "Swedish", "SE" → Q34.
    for given in strings(args, "citizenship")? {
        if sel::normalise("citizenship", &given, scope).is_ok() {
            traits.push(format!("citizenship:{given}"));
            continue;
        }
        let item = names::country(&given).and_then(|code| Some((names::country_item(code)?, code)));
        let Some((item, code)) = item else {
            return Err(bad(format!(
                "citizenship {given:?}: a country's name or ISO code, or a Q-id from den_filter_values kind=citizenship"
            )));
        };
        read_as
            .push(format!("citizenship {given} as {} ({item})", names::country_name(code).unwrap_or(code)));
        traits.push(format!("citizenship:{item}"));
    }
    traits.extend(strings(args, "occupation")?.into_iter().map(|id| format!("occupation:{id}")));
    // An age is a birth year counted back from this year; a range of either is the decades it spans, one question
    // each, and the people in them held to the exact years. An open end is bounded: no one older than 100 is
    // looked for by age, and no one born before 1900.
    let (age_min, age_max) = (integer(args, "age_min")?, integer(args, "age_max")?);
    let born_min = match (integer(args, "born_min")?, age_max) {
        (Some(year), _) => Some(year),
        (None, Some(age)) => Some(ctx.year - age - 1),
        _ => None,
    };
    let born_max = match (integer(args, "born_max")?, age_min) {
        (Some(year), _) => Some(year),
        (None, Some(age)) => Some(ctx.year - age),
        _ => None,
    };
    let living = args.get("living").and_then(Value::as_bool).unwrap_or(false);
    let sort = string(args, "sort")?.unwrap_or("prominence");
    let order = match sort {
        "prominence" => "prominence",
        "credits" => "credits",
        "name" => "name",
        "youngest" => "born_desc",
        "oldest" => "born_asc",
        _ => return Err(bad("sort is prominence, credits, name, youngest or oldest")),
    };
    let traits = sel::items(&traits, scope).map_err(bad)?;
    let labels_of = |answer: &Value| answer.get("labels").cloned().unwrap_or(Value::Null);
    // Whether atlas ordered as asked: it names the order it used. One that names none predates ordering, and
    // ordered by credits.
    let honoured = |answer: &Value| answer.get("order").and_then(Value::as_str) == Some(order);
    let (mut people, labels, total, answer, sorted) = match (born_min, born_max) {
        (None, None) => {
            let (skip, limit) = sel::page(page, limit);
            let url = sel::people_url(scope, &sel, &traits, order, Some((skip, limit)));
            let (answer, _) = ctx.atlas.get(&url, ctx.rid).await?;
            let people = answer.get("people").and_then(Value::as_array).cloned().unwrap_or_default();
            let sorted = honoured(&answer);
            (people, labels_of(&answer), answer.get("total").and_then(Value::as_u64), answer, sorted)
        }
        (min, max) => {
            let max = max.unwrap_or(ctx.year);
            let min = min.unwrap_or(if age_min.is_some() { ctx.year - 101 } else { 1900 });
            if min > max {
                return Err(bad("the birth-year range is empty"));
            }
            let decades: Vec<i64> = (min.div_euclid(10)..=max.div_euclid(10)).map(|d| d * 10).collect();
            if decades.len() as i64 > MAX_DECADES {
                return Err(bad(format!("an age range spans at most {MAX_DECADES} decades")));
            }
            // Every decade's first pages, merged in the asked order and cut to the page: a page further in than the
            // first hundred of any decade is not offered.
            let want = (page + 1) * limit;
            if want > sel::MAX_PAGE {
                return Err(bad("with an age range, page × limit stays within 100 people"));
            }
            let mut merged: Vec<(usize, Value)> = Vec::new();
            let mut labels = Map::new();
            let mut total = 0;
            let mut last = Value::Null;
            let mut sorted = true;
            for decade in decades {
                let mut with_born = traits.clone();
                with_born.extend(sel::items(&[format!("born:{decade}")], scope).map_err(bad)?);
                with_born.sort();
                let url = sel::people_url(scope, &sel, &with_born, order, Some((0, want)));
                let (answer, _) = ctx.atlas.get(&url, ctx.rid).await?;
                total += answer.get("total").and_then(Value::as_u64).unwrap_or(0);
                if let Some(l) = answer.get("labels").and_then(Value::as_object) {
                    labels.extend(l.clone());
                }
                sorted &= honoured(&answer);
                let list = answer.get("people").and_then(Value::as_array).cloned().unwrap_or_default();
                merged.extend(list.into_iter().enumerate());
                last = answer;
            }
            merged.retain(|(_, p)| {
                p.get("born").and_then(exact_year).is_none_or(|year| (min..=max).contains(&year))
            });
            // Prominence has no number to compare across decades, so each decade's order is kept and the decades
            // are interleaved by rank: the most prominent of each, then the next of each.
            let effective = if sorted { order } else { "credits" };
            merged.sort_by(|(ra, a), (rb, b)| {
                let credits = |p: &Value| p.get("credits").and_then(Value::as_u64).unwrap_or(0);
                let year = |p: &Value| p.get("born").and_then(|b| b.get("year")).and_then(Value::as_i64);
                let name = |p: &Value| p.get("name").and_then(Value::as_str).unwrap_or("").to_lowercase();
                let by = match effective {
                    "prominence" => ra.cmp(rb),
                    "name" => name(a).cmp(&name(b)),
                    "born_desc" => year(b).cmp(&year(a)),
                    "born_asc" => year(a).unwrap_or(i64::MAX).cmp(&year(b).unwrap_or(i64::MAX)),
                    _ => credits(b).cmp(&credits(a)),
                };
                by.then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
            });
            let people: Vec<Value> =
                merged.into_iter().map(|(_, p)| p).skip(page * limit).take(limit).collect();
            (people, Value::Object(labels), Some(total), last, sorted)
        }
    };
    let dropped_dead = if living {
        let before = people.len();
        people.retain(|p| p.get("died").is_none_or(Value::is_null));
        before - people.len()
    } else {
        0
    };
    let label = |id: &Value| -> Value {
        id.as_str().and_then(|q| labels.get(q)).cloned().unwrap_or_else(|| id.clone())
    };
    let results: Vec<Value> = people
        .iter()
        .filter_map(|p| {
            let name = p.get("name").and_then(Value::as_str);
            let mut out = Map::new();
            out.insert("id".into(), qid(p.get("id"))?);
            put(&mut out, "name", text(p.get("name")));
            put(&mut out, "credits", count(p.get("credits")));
            put(&mut out, "roles", texts(p.get("roles"), 8));
            for (key, cap) in [("gender", 4), ("citizenship", 4), ("occupation", 4)] {
                let ids: Vec<Value> = p
                    .get(key)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|id| qid(Some(id)))
                    .take(cap)
                    .map(|id| text(Some(&label(&id))).unwrap_or(id))
                    .collect();
                if !ids.is_empty() {
                    out.insert(key.into(), Value::Array(ids));
                }
            }
            let born = p.get("born");
            if let Some(v) = born.and_then(date) {
                out.insert("born".into(), v);
            }
            if let Some(v) = p.get("died").and_then(date) {
                out.insert("died".into(), v);
            } else if let Some(year) = born.and_then(exact_year) {
                out.insert("age".into(), json!(ctx.year - year));
            }
            if let Some(url) = person_url(ctx.cfg, p.get("tmdbId").and_then(Value::as_u64), name) {
                out.insert("url".into(), json!(url));
            }
            Some(Value::Object(out))
        })
        .collect();
    let mut out = listing(results, total, page, limit);
    said(&mut out, read_as);
    let mut notes =
        vec!["Credits and traits are Wikidata's; a person with no record for a trait is not matched, so \
                          this list is not complete."
            .to_owned()];
    if born_min.is_some() || born_max.is_some() {
        notes.push(
            "Ages are this year minus the birth year, so ±1; people dated only to a decade are kept when \
                    the decade overlaps. The total counts whole decades."
                .to_owned(),
        );
        if !living {
            notes.push("People who have died match too; pass living: true to leave them out.".to_owned());
        }
    }
    if dropped_dead > 0 {
        notes.push(format!("{dropped_dead} who have died were left out of this page, so it may be short."));
    }
    notes.push(if sorted {
        format!("Sorted by {sort}.")
    } else {
        format!("Sorted by credits (most matching titles): this Den's index can't sort people by {sort} yet.")
    });
    out.insert("notes".into(), json!(notes));
    // How many of the credited people each applied trait is on record for: what "not complete" amounts to.
    if let Some(coverage) = answer.get("traitCoverage").and_then(Value::as_object) {
        let shares: Map<String, Value> = coverage
            .iter()
            .filter_map(|(trait_, c)| {
                let (n, of) = (c.get("count")?.as_f64()?, c.get("denominator")?.as_f64()?);
                (of > 0.0)
                    .then(|| (trait_.clone(), json!(format!("{:.0}% of credited people", n / of * 100.0))))
            })
            .collect();
        if !shares.is_empty() {
            out.insert("on_record".into(), Value::Object(shares));
        }
    }
    caveats(ctx, &mut out, &answer);
    Ok(Value::Object(out))
}

// ---- den_title

fn title_key(v: &Value) -> Result<(&str, u64), ToolError> {
    let kind = v.get("type").and_then(Value::as_str).filter(|t| matches!(*t, "movie" | "series"));
    let id = v.get("id").and_then(|i| i.as_u64().or_else(|| i.as_str()?.parse().ok()));
    match (kind, id) {
        (Some(kind), Some(id)) if id > 0 => Ok((kind, id)),
        _ => Err(bad("a title is {type: movie|series, id} as another Den tool gave it")),
    }
}

async fn title_facts(ctx: &Ctx<'_>, args: &Value) -> Answer {
    let (kind, id) = title_key(args)?;
    let (answer, _) = ctx.atlas.get(&format!("/index/title/{kind}/{id}.json"), ctx.rid).await?;
    if answer.get("indexed") != Some(&json!(true)) {
        return Ok(json!({
            "type": kind, "id": id, "indexed": false,
            "note": "Den's index has nothing on this title; it may still exist.",
        }));
    }
    let mut out = match title(ctx.cfg, &answer) {
        Some(Value::Object(map)) => map,
        _ => return Err(bad("Den's index answered without a title")),
    };
    out.insert("indexed".into(), json!(true));
    if let Some(labels) = answer.get("labels") {
        put(&mut out, "animated", boolean(labels.get("animated")));
        put(&mut out, "subgenres", texts(labels.get("subgenres"), 16));
        put(&mut out, "moods", texts(labels.get("moods"), 16));
    }
    put(&mut out, "plot_facets", text_map(answer.get("plotFacets")));
    put(&mut out, "countries", texts(answer.get("countries"), 16));
    put(&mut out, "languages", texts(answer.get("languages"), 16));
    put(&mut out, "runtime_minutes", count(answer.get("runtimeMinutes")));
    put(&mut out, "based_on", texts(answer.get("basedOn"), 8));
    // The makers with their Den Web pages; the cast — up to sixty, in no billing order — by name and id alone, to
    // keep the answer lean.
    let people = |key: &str, linked: bool| -> Vec<Value> {
        answer
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|p| {
                let name = text(p.get("name"));
                let mut person = json!({ "id": qid(p.get("id"))?, "name": name });
                let tmdb = p.get("tmdbId").and_then(Value::as_u64).filter(|_| linked);
                if let Some(url) = person_url(ctx.cfg, tmdb, name.as_ref().and_then(Value::as_str)) {
                    person["url"] = json!(url);
                }
                Some(person)
            })
            .collect()
    };
    out.insert("directors_writers".into(), Value::Array(people("makers", true)));
    // Den's index names a title's makers as one list; which of them directed and which wrote, it doesn't say.
    out.insert(
        "makers_note".into(),
        json!("directors_writers is one list: Den's index doesn't say which of them directed and which wrote."),
    );
    out.insert("cast".into(), Value::Array(people("cast", false)));
    put(&mut out, "cast_total", count(answer.get("castTotal")));
    // Its iconic studios, from their own route; one atlas without it, or a title with none, has no field.
    if let Ok((studios, _)) = ctx.atlas.get(&format!("/index/studios/{kind}/{id}.json"), ctx.rid).await {
        let listed: Vec<Value> = studios
            .get("studios")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|s| Some(json!({ "id": qid(s.get("id"))?, "name": text(s.get("name"))? })))
            .collect();
        if !listed.is_empty() {
            out.insert("studios".into(), Value::Array(listed));
        }
    }
    out.insert("attribution".into(), json!(ATTRIBUTION));
    Ok(Value::Object(out))
}

// ---- den_similar

async fn similar(ctx: &Ctx<'_>, args: &Value) -> Answer {
    let seeds: Vec<(&str, u64)> = match args.get("titles") {
        Some(Value::Array(list)) if !list.is_empty() && list.len() <= 8 => {
            list.iter().map(title_key).collect::<Result<_, _>>()?
        }
        _ => return Err(bad("titles: one to eight {type, id}")),
    };
    let mix = args.get("mix_types").and_then(Value::as_bool).unwrap_or(true);
    let (page, limit) = paging(args, 20)?;
    // The narrowing is the same for every seed: `sel` carries no `like`, the one kind whose spelling depends on scope.
    let (narrowing, read_as) = selection(ctx, args, Scope::All).await?;
    let ask = |kind: &str, id: u64| -> Result<String, ToolError> {
        let scope = if mix { Scope::All } else { Scope::parse(kind).unwrap_or(Scope::All) };
        let like = if mix { format!("like:{kind}-{id}") } else { format!("like:{id}") };
        let mut sel = narrowing.clone();
        sel.extend(sel::items(&[like], scope).map_err(bad)?);
        sel.sort();
        Ok(match seeds.len() {
            1 => {
                let (skip, limit) = sel::page(page, limit);
                sel::titles_url(scope, &sel, skip, limit)
            }
            _ => sel::titles_url(scope, &sel, 0, ((page + 1) * limit).min(sel::MAX_PAGE)),
        })
    };
    if seeds.len() == 1 {
        let (kind, id) = seeds[0];
        let (answer, _) = ctx.atlas.get(&ask(kind, id)?, ctx.rid).await?;
        let mut out = listing(
            titles(ctx.cfg, answer.get("titles")),
            answer.get("total").and_then(Value::as_u64),
            page,
            limit,
        );
        // More Like This is a ranked row of a few hundred titles, and `sel` keeps only those of them it matches:
        // a narrow one leaves few. Say how to widen it rather than let a short list read as all there is.
        let short = answer.get("total").and_then(Value::as_u64).is_some_and(|t| (t as usize) < limit);
        if short && args.get("sel").is_some_and(|s| !s.is_null()) {
            out.insert(
                "hint".into(),
                json!("Only the titles most like it are filtered here. For more, den_title gives its subgenres \
                       and moods: pass those with the same sel to den_filter_titles, or den_search in words."),
            );
        }
        said(&mut out, read_as);
        caveats(ctx, &mut out, &answer);
        return Ok(Value::Object(out));
    }
    if (page + 1) * limit > sel::MAX_PAGE {
        return Err(bad("with several titles, page × limit stays within 100"));
    }
    // Each seed's row, interleaved: the first of each, then the second of each, so no one seed crowds out the
    // others. A title in several rows is listed once, where it first comes up; the seeds themselves never are.
    let mut rows = Vec::with_capacity(seeds.len());
    for &(kind, id) in &seeds {
        let (answer, _) = ctx.atlas.get(&ask(kind, id)?, ctx.rid).await?;
        rows.push(titles(ctx.cfg, answer.get("titles")));
    }
    let key = |t: &Value| (t["type"].as_str().unwrap_or("").to_owned(), t["id"].as_u64().unwrap_or(0));
    let mut seen: std::collections::HashSet<(String, u64)> =
        seeds.iter().map(|&(k, id)| (k.to_owned(), id)).collect();
    let mut merged = Vec::new();
    for i in 0..rows.iter().map(Vec::len).max().unwrap_or(0) {
        for row in &rows {
            if let Some(t) = row.get(i) {
                if seen.insert(key(t)) {
                    merged.push(t.clone());
                }
            }
        }
    }
    let results: Vec<Value> = merged.into_iter().skip(page * limit).take(limit).collect();
    let mut out = listing(results, None, page, limit);
    said(&mut out, read_as);
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_den_webs() {
        assert_eq!(slug("Fight Club"), "fight-club");
        assert_eq!(slug("Amélie"), "amelie");
        assert_eq!(slug("  -- Heat!! "), "heat");
        assert_eq!(
            slug("Dr. Strangelove or: How I Learned to Stop Worrying and Love the Bomb"),
            "dr-strangelove-or-how-i-learned-to-stop-worrying-and-love",
            "a cut word is dropped whole"
        );
        assert_eq!(slug("千と千尋の神隠し"), "");
    }

    #[test]
    fn dates_read_as_wikidata_states_them() {
        assert_eq!(
            date(&json!({"precision": "day", "date": "1974-07-29", "year": 1974})),
            Some(json!("1974-07-29"))
        );
        assert_eq!(date(&json!({"precision": "year", "year": 1980})), Some(json!(1980)));
        assert_eq!(date(&json!({"precision": "decade", "year": 1970})), Some(json!("1970s")));
        assert_eq!(date(&json!({"precision": "century", "century": 20})), Some(json!("20th century")));
        assert_eq!(date(&json!({"precision": "century", "century": 21})), Some(json!("21st century")));
        assert_eq!(exact_year(&json!({"precision": "decade", "year": 1970})), None);
    }

    #[test]
    fn tmdb_kinds_are_refused_however_they_are_written() {
        let tmdb: Vec<String> = vec!["rating".into(), "character".into(), "future".into()];
        for sel in ["rating:7", "character:walter white", "future:x", "-Rating:8", "genre:80, rating:7"] {
            assert!(refuse_tmdb_kinds(&[sel.to_owned()], &tmdb).is_err(), "{sel}");
        }
        assert!(refuse_tmdb_kinds(&["country:se".to_owned()], &tmdb).is_ok());
    }

    #[test]
    fn the_nearest_values_are_the_ones_a_person_meant() {
        let known: Vec<(String, Option<String>)> =
            ["Tense/Edge-of-seat", "Feel-good", "Slow-burn", "Dark & Gritty"]
                .map(|k| (k.to_owned(), None))
                .to_vec();
        assert_eq!(nearest(&sel::name_key("tense"), &known)[0], "Tense/Edge-of-seat");
        assert_eq!(nearest(&sel::name_key("slowburn"), &known)[0], "Slow-burn");
        assert_eq!(edit_distance("kitten", "sitting"), 3);
    }

    #[test]
    fn prose_under_any_spelling_of_a_refused_key_is_refused() {
        assert!(
            guard(json!({ "results": [{ "title": "Plot", "genre": "Overview" }] })).is_ok(),
            "values pass"
        );
        for key in ["overview", "Plot_Summary", "tag-line", "DESCRIPTION", "premise"] {
            assert!(guard(json!({ "results": [{ key: "text" }] })).is_err(), "{key}");
        }
        assert!(guard(json!({ "plot_facets": { "ending": "tragic" } })).is_ok(), "a facet is not prose");
    }

    #[test]
    fn counts_read_with_thousands() {
        assert_eq!(thousands(47_613), "47,613");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000_000), "1,000,000");
    }
}
