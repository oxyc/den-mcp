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
    fn from(f: Failed) -> Self {
        match f.status {
            Some(400) => ToolError(format!("Den refused the question: {}", f.detail)),
            Some(404) => ToolError("Den's index does not answer that question.".into()),
            Some(503) => ToolError("Den's index is loading or busy; try again in a few seconds.".into()),
            _ => ToolError(format!("Den's index is unavailable ({})", f.detail)),
        }
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
        "description": "Filters ANDed together, each \"kind:id\" (\"-kind:id\" excludes). Kinds: language (ISO 639-1), \
            country (ISO 3166-1), region (nordic, scandinavian, east-asian, …), decade (1990), primary (Den's primary \
            genre: Crime, Drama, …), subgenre and mood (Den labels, exact case: \"Psychological Thriller\", \"Tense\"), \
            genre (TMDB genre id), animated (yes|no), runtime (under-90|90-120|120-150|over-150), source (book, play, \
            comic, game, …), plot facets ending|tone|pacing|era|setting|scope|chronology|continuity|conflict|ensemble|\
            timespan|archetype, technique, audience, critique, warning, and Wikidata Q-ids for person, made (director/\
            writer/creator), cast, company, network, subject, place, format. Resolve names and exact labels with \
            den_filter_values first.",
    });
    let type_all = json!({ "type": "string", "enum": ["movie", "series", "all"], "default": "all" });
    let paging = |default: u32, max: u32| {
        (
            json!({ "type": "integer", "minimum": 0, "default": 0, "description": "0-based page" }),
            json!({ "type": "integer", "minimum": 1, "maximum": max, "default": default }),
        )
    };
    let (page, limit20) = paging(20, 100);
    let (_, limit10) = paging(10, 50);
    let read_only = json!({ "readOnlyHint": true, "openWorldHint": false });
    json!([
        {
            "name": "den_search",
            "title": "Search Den",
            "description": "Search Den's film and series index in plain words: a title, a person, \"the one where …\", \
                a mood or theme (\"slow-burn 90s thrillers\", \"bleak korean heist\"). Den reads the words itself; put \
                a year window or language you worked out in the parameters. Returns titles best first, what Den \
                understood, and people the words named. For exact criteria use den_filter_titles.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "minLength": 2 },
                    "type": { "type": "string", "enum": ["movie", "series"] },
                    "year_min": { "type": "integer" },
                    "year_max": { "type": "integer" },
                    "language": { "type": "string", "description": "Original language, ISO 639-1" },
                    "runtime_max": { "type": "integer", "description": "Minutes; films only" },
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
                Chain: den_filter_values to turn names into ids (\"Swedish\" → language:sv, \"Christopher Nolan\" → \
                made:Q25191), then this.",
            "inputSchema": {
                "type": "object",
                "properties": { "type": type_all, "sel": sel_items, "page": page, "limit": limit20 },
            },
            "annotations": read_only,
        },
        {
            "name": "den_filter_values",
            "title": "Look up filter values",
            "description": "Resolve a name to the id a filter takes, with how many titles carry it under `sel`. \
                `kind` is any den_filter_titles kind, or a person trait for den_find_people: gender, citizenship, \
                occupation, born (decade), role. `q` matches the start of a word in the name (\"nol\" → Christopher \
                Nolan). Without `kind`: an overview of the commonest values of every kind under `sel`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kind": { "type": "string" },
                    "q": { "type": "string" },
                    "type": type_all,
                    "sel": sel_items,
                    "limit": { "type": "integer", "minimum": 1, "maximum": 30, "default": 10 },
                },
            },
            "annotations": read_only,
        },
        {
            "name": "den_find_people",
            "title": "Find people",
            "description": "People credited on the titles `sel` matches (e.g. [\"decade:2020\"] for 2020s titles), \
                filtered by what Wikidata states about them, most matching credits first. Ages are from birth \
                years, today. Resolve citizenship/occupation Q-ids with den_filter_values. People Wikidata has no \
                record for on a trait are left out, so a list is never complete; say so.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "type": type_all,
                    "sel": sel_items,
                    "role": { "type": "string", "enum": ["cast", "director", "writer", "creator"] },
                    "gender": { "type": "string", "description": "male, female, non-binary, or a Wikidata Q-id" },
                    "age_min": { "type": "integer" },
                    "age_max": { "type": "integer" },
                    "born_min": { "type": "integer", "description": "Birth year, instead of an age" },
                    "born_max": { "type": "integer" },
                    "citizenship": { "type": "array", "items": { "type": "string" }, "description": "Q-ids, all held" },
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
                tone, pacing, …), countries, languages, runtime, what it adapts, directors/writers and cast with \
                their Wikidata ids. No plot summary, ratings or availability: share its url for those.",
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

/// A title selection, refusing what a tool does not offer: the kinds that are TMDB's (ratings, characters), `like`,
/// which is den_similar's, and the person traits, which are den_find_people's.
fn selection(args: &Value, scope: Scope, tmdb_kinds: &[String]) -> Result<Vec<Item>, ToolError> {
    let items = sel::items(&strings(args, "sel")?, scope).map_err(bad)?;
    for item in &items {
        let kind = item.kind.as_str();
        if tmdb_kinds.iter().any(|k| k == kind) {
            return Err(bad(format!(
                "{kind} is not offered here; a title's url shows what Den Web has on it"
            )));
        }
        if kind == "like" {
            return Err(bad("use den_similar for titles like another"));
        }
        if sel::TRAITS.contains(&kind) {
            return Err(bad(format!("{kind} is a person trait: pass it to den_find_people")));
        }
    }
    Ok(items)
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
    if let Some(name) = name {
        out.insert("title".into(), json!(name));
    }
    for (from, to) in [("year", "year"), ("primaryGenre", "genre"), ("originalLanguage", "language")] {
        if let Some(v) = card.get(from).filter(|v| !v.is_null() && v.as_str() != Some("")) {
            out.insert(to.into(), v.clone());
        }
    }
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
fn caveats(out: &mut Map<String, Value>, answer: &Value) {
    for (field, key) in [
        ("ignored", "ignored"),
        ("unknownValues", "unknown_values"),
        ("kindsUnavailable", "kinds_unavailable"),
        ("ignoredTraits", "ignored_traits"),
        ("unknownTraits", "unknown_traits"),
        ("traitsUnavailable", "traits_unavailable"),
    ] {
        // A TMDB kind this tool never offers is nothing to tell a model about.
        let listed: Vec<Value> = answer
            .get(field)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|v| !v.as_str().is_some_and(|k| crate::atlas::TMDB_KINDS.contains(&k)))
            .cloned()
            .collect();
        if !listed.is_empty() {
            out.insert(key.into(), Value::Array(listed));
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
        if language.len() != 2 || !language.bytes().all(|b| b.is_ascii_alphabetic()) {
            return Err(bad("language is an ISO 639-1 code, e.g. sv"));
        }
        url.push_str(&format!("&language={}", language.to_ascii_lowercase()));
    }
    url.push_str(&format!("&skip={}&limit={limit}", page * limit));
    let (answer, _) = ctx.atlas.get(&url, ctx.rid).await?;
    // A hit the corpus has no card for is named by TMDB's export (`titleFrom: "tmdb"`): its name is TMDB's, so it
    // is left out, and the answer says how many were.
    let hits = answer.get("hits").and_then(Value::as_array).cloned().unwrap_or_default();
    let (named, tmdb): (Vec<Value>, Vec<Value>) =
        hits.into_iter().partition(|h| h.get("titleFrom").and_then(Value::as_str) != Some("tmdb"));
    let results = titles(ctx.cfg, Some(&Value::Array(named)));
    let mut out = listing(results, answer.get("total").and_then(Value::as_u64), page, limit);
    if !tmdb.is_empty() {
        out.insert(
            "left_out".into(),
            json!(format!("{} matches Den has no description of, only a TMDB name", tmdb.len())),
        );
    }
    let not_applied: Vec<Value> = ["ignored", "unknownValues"]
        .iter()
        .filter_map(|k| answer.get(*k)?.as_array())
        .flatten()
        .cloned()
        .collect();
    if !not_applied.is_empty() {
        out.insert("not_applied".into(), Value::Array(not_applied));
        out.insert("note".into(), json!("Part of the question was not applied: see not_applied."));
    }
    let mut understood = Map::new();
    if let Some(parse) = answer.get("parse").and_then(Value::as_object) {
        for (from, to) in [
            ("mediaType", "type"),
            ("country", "country"),
            ("decade", "decade"),
            ("yearMin", "year_min"),
            ("yearMax", "year_max"),
            ("language", "language"),
            ("runtimeMax", "runtime_max"),
            ("labels", "labels"),
            ("plotFacets", "plot_facets"),
            ("basedOnKind", "based_on"),
            ("leftover", "theme"),
        ] {
            let v = parse.get(from).filter(|v| match v {
                Value::Null => false,
                Value::Array(a) => !a.is_empty(),
                Value::String(s) => !s.is_empty(),
                _ => true,
            });
            if let Some(v) = v {
                understood.insert(to.into(), v.clone());
            }
        }
        if let Some(genres) = parse.get("genres").and_then(Value::as_array).filter(|g| !g.is_empty()) {
            let named: Vec<Value> = genres
                .iter()
                .filter_map(Value::as_u64)
                .map(|g| json!(genre_name(g).unwrap_or("?")))
                .collect();
            understood.insert("genres".into(), Value::Array(named));
        }
        let phrases = answer.pointer("/parse/excluded/phrases");
        if let Some(phrases) = phrases.filter(|p| p.as_array().is_some_and(|a| !a.is_empty())) {
            understood.insert("excluded".into(), phrases.clone());
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
            let mut person = json!({ "id": p.get("qid")?, "name": name, "credits": p.get("credits") });
            if let Some(url) = person_url(ctx.cfg, p.get("id").and_then(Value::as_u64), name) {
                person["url"] = json!(url);
            }
            Some(person)
        })
        .collect();
    if !people.is_empty() {
        out.insert("people".into(), Value::Array(people));
    }
    Ok(Value::Object(out))
}

/// TMDB's genre names, for the ids Den's `genre` kind and search's reading of words are written in.
pub fn genre_name(id: u64) -> Option<&'static str> {
    Some(match id {
        28 => "Action",
        12 => "Adventure",
        16 => "Animation",
        35 => "Comedy",
        80 => "Crime",
        99 => "Documentary",
        18 => "Drama",
        10751 => "Family",
        14 => "Fantasy",
        36 => "History",
        27 => "Horror",
        10402 => "Music",
        9648 => "Mystery",
        10749 => "Romance",
        878 => "Science Fiction",
        10770 => "TV Movie",
        53 => "Thriller",
        10752 => "War",
        37 => "Western",
        10759 => "Action & Adventure",
        10762 => "Kids",
        10763 => "News",
        10764 => "Reality",
        10765 => "Sci-Fi & Fantasy",
        10766 => "Soap",
        10767 => "Talk",
        10768 => "War & Politics",
        _ => return None,
    })
}

// ---- den_filter_titles

async fn filter_titles(ctx: &Ctx<'_>, args: &Value) -> Answer {
    let scope = scope(args)?;
    let sel = selection(args, scope, ctx.tmdb_kinds)?;
    let (page, limit) = paging(args, 20)?;
    let (skip, limit) = sel::page(page, limit);
    let (answer, _) = ctx.atlas.get(&sel::titles_url(scope, &sel, skip, limit), ctx.rid).await?;
    let results = titles(ctx.cfg, answer.get("titles"));
    let mut out = listing(results, answer.get("total").and_then(Value::as_u64), page, limit);
    out_of(&mut out, answer.get("denominator"));
    caveats(&mut out, &answer);
    Ok(Value::Object(out))
}

// ---- den_filter_values

/// How many of a kind's commonest values the overview lists.
const OVERVIEW_TOP: usize = 8;

async fn filter_values(ctx: &Ctx<'_>, args: &Value) -> Answer {
    let scope = scope(args)?;
    let sel = selection(args, scope, ctx.tmdb_kinds)?;
    let limit = integer(args, "limit")?.unwrap_or(10).clamp(1, 30) as usize;
    let q = string(args, "q")?;
    let Some(kind) = string(args, "kind")?.map(str::to_ascii_lowercase) else {
        return overview(ctx, scope, &sel).await;
    };
    if ctx.tmdb_kinds.contains(&kind) {
        return Err(bad(format!("{kind} is not offered here")));
    }
    if kind == "like" {
        return Err(bad("use den_similar for titles like another"));
    }
    if sel::TRAITS.contains(&kind.as_str()) {
        return trait_values(ctx, scope, &sel, &kind, q, limit).await;
    }
    if let Some(q) = q.filter(|q| sel::name_key(q).chars().count() < 2) {
        return Err(bad(format!("q {q:?}: at least 2 letters")));
    }
    let (answer, _) = ctx.atlas.get(&sel::values_url(scope, &kind, &sel, q, limit), ctx.rid).await?;
    let values: Vec<Value> = answer
        .get("values")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| {
            let id = v.get("id")?.as_str()?;
            let name = match (kind.as_str(), id.parse::<u64>().ok().and_then(genre_name)) {
                ("genre", Some(name)) => name.to_owned(),
                _ => v.get("name").and_then(Value::as_str).unwrap_or(id).to_owned(),
            };
            Some(json!({ "id": id, "name": name, "count": v.get("count") }))
        })
        .collect();
    let mut out = Map::new();
    out.insert("kind".into(), json!(kind));
    out.insert("use_as".into(), json!(format!("\"{kind}:<id>\" in sel")));
    out.insert("values".into(), Value::Array(values));
    if let Some(complete) = answer.get("complete") {
        out.insert("complete".into(), complete.clone());
    }
    out_of(&mut out, answer.get("denominator"));
    caveats(&mut out, &answer);
    Ok(Value::Object(out))
}

/// Every kind's commonest values under the selection, named.
async fn overview(ctx: &Ctx<'_>, scope: Scope, sel: &[Item]) -> Answer {
    let (answer, _) = ctx.atlas.get(&sel::counts_url(scope, sel), ctx.rid).await?;
    let mut kinds = Map::new();
    for (kind, about) in answer.get("kinds").and_then(Value::as_object).into_iter().flatten() {
        if ctx.tmdb_kinds.contains(kind) {
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
            .map(|(id, n)| {
                let name =
                    labels.and_then(|l| l.get(&id)).and_then(Value::as_str).map(str::to_owned).or_else(
                        || {
                            (kind == "genre")
                                .then(|| id.parse().ok().and_then(genre_name))
                                .flatten()
                                .map(str::to_owned)
                        },
                    );
                match name {
                    Some(name) => json!({ "id": id, "name": name, "count": n }),
                    None => json!({ "id": id, "count": n }),
                }
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
    caveats(&mut out, &answer);
    Ok(Value::Object(out))
}

/// A person trait's values under the selection, from `people/counts.json`, named and matched by `q`.
async fn trait_values(
    ctx: &Ctx<'_>,
    scope: Scope,
    sel: &[Item],
    kind: &str,
    q: Option<&str>,
    limit: usize,
) -> Answer {
    let (answer, _) = ctx.atlas.get(&sel::people_url(scope, sel, &[], None), ctx.rid).await?;
    let about = answer.pointer(&format!("/traits/{kind}"));
    let labels = about.and_then(|a| a.get("labels"));
    let q = q.map(sel::name_key).filter(|q| !q.is_empty());
    let mut values: Vec<(String, String, u64)> = about
        .and_then(|a| a.get("values"))
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(id, n)| {
            let name = labels.and_then(|l| l.get(id)).and_then(Value::as_str).unwrap_or(id).to_owned();
            Some((id.clone(), name, n.as_u64()?))
        })
        .filter(|(id, name, _)| {
            q.as_deref().is_none_or(|q| {
                let key = sel::name_key(name);
                key.starts_with(q) || key.split(' ').any(|w| w.starts_with(q)) || id.eq_ignore_ascii_case(q)
            })
        })
        .collect();
    values.sort_by(|a, b| b.2.cmp(&a.2).then(a.1.cmp(&b.1)));
    let total = values.len();
    let listed: Vec<Value> = values
        .into_iter()
        .take(limit)
        .map(|(id, name, n)| json!({ "id": id, "name": name, "people": n }))
        .collect();
    let mut out = Map::new();
    out.insert("kind".into(), json!(kind));
    out.insert("use_as".into(), json!(format!("den_find_people's {kind}")));
    out.insert("values".into(), Value::Array(listed));
    out.insert("complete".into(), json!(total <= limit));
    caveats(&mut out, &answer);
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
    let sel = selection(args, scope, ctx.tmdb_kinds)?;
    let (page, limit) = paging(args, 20)?;
    let mut traits: Vec<String> = Vec::new();
    if let Some(role) = string(args, "role")? {
        traits.push(format!("role:{role}"));
    }
    if let Some(gender) = string(args, "gender")? {
        traits.push(format!("gender:{}", gender_id(gender)?));
    }
    for (key, kind) in [("citizenship", "citizenship"), ("occupation", "occupation")] {
        traits.extend(strings(args, key)?.into_iter().map(|id| format!("{kind}:{id}")));
    }
    // An age is a birth year counted back from this year; a range of either is the decades it spans, one question
    // each, and the people in them held to the exact years.
    let born_min = match (integer(args, "born_min")?, integer(args, "age_max")?) {
        (Some(year), _) => Some(year),
        (None, Some(age)) => Some(ctx.year - age - 1),
        _ => None,
    };
    let born_max = match (integer(args, "born_max")?, integer(args, "age_min")?) {
        (Some(year), _) => Some(year),
        (None, Some(age)) => Some(ctx.year - age),
        _ => None,
    };
    let traits = sel::items(&traits, scope).map_err(bad)?;
    let labels_of = |answer: &Value| answer.get("labels").cloned().unwrap_or(Value::Null);
    let (people, labels, total, answer) = match (born_min, born_max) {
        (None, None) => {
            let (skip, limit) = sel::page(page, limit);
            let (answer, _) =
                ctx.atlas.get(&sel::people_url(scope, &sel, &traits, Some((skip, limit))), ctx.rid).await?;
            let people = answer.get("people").and_then(Value::as_array).cloned().unwrap_or_default();
            (people, labels_of(&answer), answer.get("total").and_then(Value::as_u64), answer)
        }
        (min, max) => {
            let (min, max) = (min.unwrap_or(max.unwrap_or(0) - 120), max.unwrap_or(ctx.year));
            if min > max {
                return Err(bad("the birth-year range is empty"));
            }
            let decades: Vec<i64> = (min.div_euclid(10)..=max.div_euclid(10)).map(|d| d * 10).collect();
            if decades.len() as i64 > MAX_DECADES {
                return Err(bad(format!("an age range spans at most {MAX_DECADES} decades")));
            }
            // Every decade's first pages, merged by credits and cut to the page: a page further in than the first
            // hundred of any decade is not offered.
            let want = (page + 1) * limit;
            if want > sel::MAX_PAGE {
                return Err(bad("with an age range, page × limit stays within 100 people"));
            }
            let mut merged: Vec<Value> = Vec::new();
            let mut labels = Map::new();
            let mut total = 0;
            let mut last = Value::Null;
            for decade in decades {
                let mut with_born = traits.clone();
                with_born.extend(sel::items(&[format!("born:{decade}")], scope).map_err(bad)?);
                with_born.sort();
                let url = sel::people_url(scope, &sel, &with_born, Some((0, want)));
                let (answer, _) = ctx.atlas.get(&url, ctx.rid).await?;
                total += answer.get("total").and_then(Value::as_u64).unwrap_or(0);
                if let Some(l) = answer.get("labels").and_then(Value::as_object) {
                    labels.extend(l.clone());
                }
                merged.extend(answer.get("people").and_then(Value::as_array).cloned().unwrap_or_default());
                last = answer;
            }
            merged.retain(|p| {
                p.get("born").and_then(exact_year).is_none_or(|year| (min..=max).contains(&year))
            });
            merged.sort_by(|a, b| {
                let credits = |p: &Value| p.get("credits").and_then(Value::as_u64).unwrap_or(0);
                credits(b).cmp(&credits(a)).then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
            });
            let people: Vec<Value> = merged.into_iter().skip(page * limit).take(limit).collect();
            (people, Value::Object(labels), Some(total), last)
        }
    };
    let label = |id: &Value| -> Value {
        id.as_str().and_then(|q| labels.get(q)).cloned().unwrap_or_else(|| id.clone())
    };
    let results: Vec<Value> = people
        .iter()
        .filter_map(|p| {
            let name = p.get("name").and_then(Value::as_str);
            let mut out = Map::new();
            out.insert("id".into(), p.get("id")?.clone());
            out.insert("name".into(), json!(name));
            for key in ["credits", "roles"] {
                if let Some(v) = p.get(key) {
                    out.insert(key.into(), v.clone());
                }
            }
            for (key, cap) in [("gender", 4), ("citizenship", 4), ("occupation", 4)] {
                if let Some(list) = p.get(key).and_then(Value::as_array).filter(|l| !l.is_empty()) {
                    out.insert(key.into(), list.iter().take(cap).map(label).collect());
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
    }
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
    caveats(&mut out, &answer);
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
        for (from, to) in [("animated", "animated"), ("subgenres", "subgenres"), ("moods", "moods")] {
            if let Some(v) = labels.get(from).filter(|v| v.as_array().is_none_or(|a| !a.is_empty())) {
                out.insert(to.into(), v.clone());
            }
        }
    }
    for (from, to) in [
        ("plotFacets", "plot_facets"),
        ("countries", "countries"),
        ("languages", "languages"),
        ("runtimeMinutes", "runtime_minutes"),
        ("basedOn", "based_on"),
    ] {
        let v = answer.get(from).filter(|v| match v {
            Value::Null => false,
            Value::Array(a) => !a.is_empty(),
            Value::Object(o) => !o.is_empty(),
            _ => true,
        });
        if let Some(v) = v {
            out.insert(to.into(), v.clone());
        }
    }
    // The makers with their Den Web pages; the cast — up to sixty, in no billing order — by name and id alone, to
    // keep the answer lean.
    let people = |key: &str, linked: bool| -> Vec<Value> {
        answer
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|p| {
                let name = p.get("name").and_then(Value::as_str);
                let mut person = json!({ "id": p.get("id")?, "name": name });
                let tmdb = p.get("tmdbId").and_then(Value::as_u64).filter(|_| linked);
                if let Some(url) = person_url(ctx.cfg, tmdb, name) {
                    person["url"] = json!(url);
                }
                Some(person)
            })
            .collect()
    };
    out.insert("directors_writers".into(), Value::Array(people("makers", true)));
    out.insert("cast".into(), Value::Array(people("cast", false)));
    if let Some(total) = answer.get("castTotal") {
        out.insert("cast_total".into(), total.clone());
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
    let ask = |kind: &str, id: u64| -> Result<String, ToolError> {
        let scope = if mix { Scope::All } else { Scope::parse(kind).unwrap_or(Scope::All) };
        let like = if mix { format!("like:{kind}-{id}") } else { format!("like:{id}") };
        let mut sel = selection(args, scope, ctx.tmdb_kinds)?;
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
        caveats(&mut out, &answer);
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
    Ok(Value::Object(listing(results, None, page, limit)))
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
    fn tmdb_kinds_like_and_traits_are_not_title_filters_here() {
        let tmdb: Vec<String> = vec!["rating".into(), "character".into(), "future".into()];
        for sel in ["rating:7", "character:walter white", "future:x", "like:movie-1", "gender:Q6581097"] {
            assert!(selection(&json!({ "sel": [sel] }), Scope::All, &tmdb).is_err(), "{sel}");
        }
        assert!(selection(&json!({ "sel": ["country:se"] }), Scope::All, &tmdb).is_ok());
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
