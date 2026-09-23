//! Atlas's filter URLs, built in their canonical spelling (den-atlas `filter::Request::parse`): the URL is the
//! cache key there and here, so the same question asked twice is one cache entry in both. A spelling atlas would
//! canonicalise differently is still answered by atlas, only kept briefly and privately — so a miss here costs
//! caching, never an answer. `testdata/facets-canonical.json` is atlas's own contract fixture, and the tests hold
//! these builders to it.

use crate::atlas::encode;

/// The most items one list (`sel` or `traits`) may carry.
pub const MAX_ITEMS: usize = 16;
/// A titles or people page when none is asked for, and the most one returns.
pub const PAGE: usize = 24;
pub const MAX_PAGE: usize = 100;

/// The kinds whose ids are integers, a decade, upper case, a label kept exactly, or a Wikidata Q-id. Every other
/// known kind is lower case.
const INTEGER: &[&str] = &["genre", "rating"];
const DECADE: &[&str] = &["decade", "born"];
const UPPER: &[&str] = &["country"];
const LABEL: &[&str] = &["mood", "subgenre", "primary"];
const QID: &[&str] = &[
    "person",
    "made",
    "cast",
    "company",
    "studio",
    "network",
    "subject",
    "place",
    "format",
    "gender",
    "citizenship",
    "occupation",
];
/// Every kind atlas knows besides those: its lower-case kinds, the plot-facet axes (den-store `FACET_AXES`) and
/// the person trait `role`.
const LOWER: &[&str] = &[
    "language",
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
    "role",
];

/// The person traits `people.json` takes as `traits`, rather than in `sel`.
pub const TRAITS: &[&str] = &["gender", "citizenship", "occupation", "born", "role"];

/// Whether atlas knows `kind`: a kind this server can spell canonically. Nothing else goes into a URL — a kind is a
/// path segment in `values/<kind>.json` and a prefix in `sel`, and one built from anything a caller sends could
/// reach another route (`../title/…`) or smuggle a parameter (`x&sel=…`).
pub fn known_kind(kind: &str) -> bool {
    [INTEGER, DECADE, UPPER, LABEL, QID, LOWER].iter().any(|list| list.contains(&kind))
        || matches!(kind, "character" | "like")
}

/// Whether `kind` is one a title filter or `values/<kind>.json` takes: known, and not a person trait. `like` is
/// among them here; the tools that must not offer it refuse it themselves.
pub fn title_kind(kind: &str) -> bool {
    known_kind(kind) && !TRAITS.contains(&kind)
}

/// How `people.json` orders people (`order`), as den-atlas takes it. `prominence` is atlas's default and so is left
/// out of the canonical URL.
pub const ORDERS: &[&str] = &["prominence", "credits", "name", "born_desc", "born_asc"];

/// Which list an item is for: the title selection (`sel`), where a `like` names its title typed only under `all`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Scope {
    Movie,
    Series,
    All,
}

impl Scope {
    pub fn parse(s: &str) -> Option<Scope> {
        match s {
            "movie" => Some(Scope::Movie),
            "series" => Some(Scope::Series),
            "all" => Some(Scope::All),
            _ => None,
        }
    }

    pub fn segment(self) -> &'static str {
        match self {
            Scope::Movie => "movie",
            Scope::Series => "series",
            Scope::All => "all",
        }
    }
}

/// One `[-]kind:id`, normalised.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Item {
    pub kind: String,
    pub exclude: bool,
    pub id: String,
}

impl Item {
    fn spelled(&self) -> String {
        // The kind is a known name of lower-case letters (`normalise`); it is encoded all the same, so nothing that
        // ever reaches here can end the item or the parameter.
        format!("{}{}:{}", if self.exclude { "-" } else { "" }, encode(&self.kind), encode(&self.id))
    }
}

/// `structure:<value>` is atlas's old axis, answered by whichever axis now holds the value.
fn resolve_axis(kind: String, id: &str) -> String {
    if kind != "structure" {
        return kind;
    }
    match id.to_ascii_lowercase().as_str() {
        "single-day" => "timespan",
        "anthology" => "continuity",
        _ => "chronology",
    }
    .to_owned()
}

/// Letters with a diacritic, folded to their base the way atlas's NFD fold does for Latin scripts. Anything else is
/// only lower-cased: atlas answers another spelling too, so this is about sharing a cache entry, not correctness.
pub fn fold_char(c: char) -> char {
    if c.is_ascii() {
        return c;
    }
    const FROM: &str = "àáâãäåāăąçćčďèéêëēėęěìíîïīįñńňòóôõöøōőŕřśšşťùúûüūůűųýÿźżžðł";
    const TO: &str = "aaaaaaaaacccdeeeeeeeeiiiiiinnnooooooooorrssstuuuuuuuuyyzzzdl";
    FROM.chars().position(|f| f == c).and_then(|i| TO.chars().nth(i)).unwrap_or(c)
}

/// A name as atlas looks it up (`facts::name_key`): folded, and its words joined by single spaces.
pub fn name_key(name: &str) -> String {
    // Most names are ASCII, which needs lower-casing and nothing else.
    let lowered = if name.is_ascii() {
        name.to_ascii_lowercase()
    } else {
        name.to_lowercase().chars().map(fold_char).collect::<String>()
    };
    lowered
        .replace('&', " and ")
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// A typed title under `all`: `movie-550`, `series-1396`.
fn typed_title(id: &str) -> Option<(&'static str, u32)> {
    let (media, n) = id.split_once('-')?;
    let media = match media.to_ascii_lowercase().as_str() {
        "movie" => "movie",
        "series" => "series",
        _ => return None,
    };
    Some((media, n.parse().ok()?))
}

/// A kind and an id as their canonical pair, or why it is refused. Atlas would keep a kind it does not know and report
/// it as ignored; here an unknown kind is refused (`known_kind`), since it would go into the URL as sent.
pub fn normalise(kind: &str, id: &str, scope: Scope) -> Result<(String, String), String> {
    let kind = kind.trim().to_ascii_lowercase();
    let id = id.trim();
    if kind.is_empty() {
        return Err(format!(":{id}: an empty kind"));
    }
    if id.is_empty() {
        return Err(format!("{kind}: an empty id"));
    }
    let kind = resolve_axis(kind, id);
    if !kind.bytes().all(|b| b.is_ascii_lowercase()) || !known_kind(&kind) {
        return Err(format!("{kind:?} is not a kind Den filters by"));
    }
    let k = kind.as_str();
    let number = || id.parse::<u32>().map_err(|_| format!("{kind}: {id:?} is not an integer"));
    let two_letters = id.len() == 2 && id.bytes().all(|b| b.is_ascii_alphabetic());
    let id = if k == "country" && !two_letters {
        // A name or demonym: "Sweden", "Swedish" → SE.
        crate::names::country(id)
            .ok_or_else(|| {
                format!("country: {id:?} names no country; use its English name or ISO 3166-1 code")
            })?
            .to_owned()
    } else if k == "language" && !two_letters {
        crate::names::language(id)
            .ok_or_else(|| {
                format!("language: {id:?} names no language; use its English name or ISO 639-1 code")
            })?
            .to_owned()
    } else if INTEGER.contains(&k) {
        number()?.to_string()
    } else if k == "born" && id.contains('-') {
        born_range(id)?
    } else if DECADE.contains(&k) {
        (number()? / 10 * 10).to_string()
    } else if UPPER.contains(&k) {
        id.to_ascii_uppercase()
    } else if LABEL.contains(&k) {
        id.to_owned()
    } else if QID.contains(&k) {
        let digits = id.strip_prefix(['Q', 'q']).unwrap_or(id);
        match digits.parse::<u32>() {
            Ok(n) if digits.bytes().all(|b| b.is_ascii_digit()) => format!("Q{n}"),
            _ => return Err(format!("{kind}: {id:?} is not a Wikidata Q-id")),
        }
    } else if k == "character" {
        let name = name_key(id);
        if name.is_empty() {
            return Err(format!("{kind}: {id:?} names no character"));
        }
        name.replace(' ', "-")
    } else if k == "like" {
        match scope {
            Scope::All => match typed_title(id) {
                Some((media, n)) => format!("{media}-{n}"),
                None => return Err(format!("like: {id:?} names no typed title; movie-<id> or series-<id>")),
            },
            _ => number()?.to_string(),
        }
    } else if LOWER.contains(&k) {
        id.to_ascii_lowercase()
    } else {
        id.to_owned()
    };
    Ok((kind, id))
}

/// The earliest birth year a `born` range may name; the latest is next year.
pub const FIRST_BIRTH_YEAR: i64 = 1800;

/// A `born` range of birth years as atlas spells it (`1976-1996`, `1976-`, `-1996`): both ends inclusive, either
/// left out, each a year from `FIRST_BIRTH_YEAR` to next year.
fn born_range(id: &str) -> Result<String, String> {
    let latest = crate::year_of(crate::unix_now()) + 1;
    let (from, to) = id.split_once('-').ok_or_else(|| format!("born: {id:?} is not <year>-<year>"))?;
    let year = |end: &str| -> Result<Option<i64>, String> {
        let end = end.trim();
        if end.is_empty() {
            return Ok(None);
        }
        let year = end
            .parse::<i64>()
            .ok()
            .filter(|_| end.bytes().all(|b| b.is_ascii_digit()))
            .ok_or_else(|| format!("born: {id:?} is not <year>-<year>"))?;
        if !(FIRST_BIRTH_YEAR..=latest).contains(&year) {
            return Err(format!("born: {year} is not a birth year from {FIRST_BIRTH_YEAR} to {latest}"));
        }
        Ok(Some(year))
    };
    let end = |year: Option<i64>| year.map_or(String::new(), |y| y.to_string());
    match (year(from)?, year(to)?) {
        (None, None) => Err(format!("born: {id:?} names no year")),
        (Some(from), Some(to)) if from > to => Err(format!("born: {id:?} ends before it starts")),
        (from, to) => Ok(format!("{}-{}", end(from), end(to))),
    }
}

/// A list of `[-]kind:id` items, normalised, sorted by kind, then positive before excluded, then id, each once. A
/// `born` range beside another positive `born` pick is refused, as atlas refuses it.
pub fn items(raw: &[String], scope: Scope) -> Result<Vec<Item>, String> {
    let mut out = Vec::with_capacity(raw.len());
    for item in raw.iter().flat_map(|i| i.split(',')).map(str::trim).filter(|i| !i.is_empty()) {
        let (exclude, item) = match item.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, item),
        };
        let (kind, id) = item.split_once(':').ok_or_else(|| format!("{item:?} is not <kind>:<id>"))?;
        let (kind, id) = normalise(kind, id, scope)?;
        out.push(Item { kind, exclude, id });
    }
    out.sort();
    out.dedup();
    if out.len() > MAX_ITEMS {
        return Err(format!("at most {MAX_ITEMS} values in one list"));
    }
    let born: Vec<&Item> = out.iter().filter(|i| i.kind == "born" && !i.exclude).collect();
    if born.len() > 1 && born.iter().any(|i| i.id.contains('-')) {
        return Err("born: one range per request, and no other born pick beside it".to_owned());
    }
    Ok(out)
}

fn joined(items: &[Item]) -> String {
    items.iter().map(Item::spelled).collect::<Vec<_>>().join(",")
}

fn query(parts: Vec<String>) -> String {
    if parts.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.join("&"))
    }
}

/// A page, as atlas takes one: `limit` within 1..=100 and `skip` a whole number of pages.
pub fn page(page: usize, limit: usize) -> (usize, usize) {
    let limit = limit.clamp(1, MAX_PAGE);
    (page.saturating_mul(limit), limit)
}

fn paging(parts: &mut Vec<String>, skip: usize, limit: usize) {
    if skip != 0 {
        parts.push(format!("skip={skip}"));
    }
    if limit != PAGE {
        parts.push(format!("limit={limit}"));
    }
}

/// `/index/filter/<scope>/titles.json`.
pub fn titles_url(scope: Scope, sel: &[Item], skip: usize, limit: usize) -> String {
    let mut parts = Vec::new();
    if !sel.is_empty() {
        parts.push(format!("sel={}", joined(sel)));
    }
    paging(&mut parts, skip, limit);
    format!("/index/filter/{}/titles.json{}", scope.segment(), query(parts))
}

/// `/index/filter/<scope>/counts.json`.
pub fn counts_url(scope: Scope, sel: &[Item]) -> String {
    let parts = if sel.is_empty() { Vec::new() } else { vec![format!("sel={}", joined(sel))] };
    format!("/index/filter/{}/counts.json{}", scope.segment(), query(parts))
}

/// `/index/filter/<scope>/values/<kind>.json`, `None` for a kind that is not a title kind (`title_kind`): the kind
/// is a path segment. `limit` is the kind's own page, which is also its most: 10, and 5 for a character.
pub fn values_url(scope: Scope, kind: &str, sel: &[Item], q: Option<&str>, limit: usize) -> Option<String> {
    if !title_kind(kind) || kind == "like" {
        return None;
    }
    let most = if kind == "character" { 5 } else { 10 };
    let limit = limit.clamp(1, most);
    let mut parts = Vec::new();
    if !sel.is_empty() {
        parts.push(format!("sel={}", joined(sel)));
    }
    // A character's name is normalised by atlas's own character rules; the plain fold is the same for any name
    // without a title or an honorific in it.
    if let Some(q) = q.map(name_key).filter(|q| !q.is_empty()) {
        parts.push(format!("q={}", encode(&q)));
    }
    if limit != most {
        parts.push(format!("limit={limit}"));
    }
    Some(format!("/index/filter/{}/values/{kind}.json{}", scope.segment(), query(parts)))
}

/// `/index/filter/<scope>/people/values/<trait>.json` (den-atlas#80), for the traits whose values are Wikidata items:
/// every value counted under the selection, found by `q` (folded, at least 2 letters). `None` for another trait.
pub fn people_values_url(
    scope: Scope,
    sel: &[Item],
    kind: &str,
    q: Option<&str>,
    limit: usize,
) -> Option<String> {
    if !matches!(kind, "gender" | "citizenship" | "occupation") {
        return None;
    }
    let most = 10;
    let limit = limit.clamp(1, most);
    let mut parts = Vec::new();
    if !sel.is_empty() {
        parts.push(format!("sel={}", joined(sel)));
    }
    if let Some(q) = q.map(name_key).filter(|q| q.chars().count() >= 2) {
        parts.push(format!("q={}", encode(&q)));
    }
    if limit != most {
        parts.push(format!("limit={limit}"));
    }
    Some(format!("/index/filter/{}/people/values/{kind}.json{}", scope.segment(), query(parts)))
}

/// `/index/filter/<scope>/people.json` in `order` (one of `ORDERS`; the default left out), or `people/counts.json`
/// with no page.
pub fn people_url(
    scope: Scope,
    sel: &[Item],
    traits: &[Item],
    order: &str,
    page: Option<(usize, usize)>,
) -> String {
    let mut parts = Vec::new();
    if !sel.is_empty() {
        parts.push(format!("sel={}", joined(sel)));
    }
    if !traits.is_empty() {
        parts.push(format!("traits={}", joined(traits)));
    }
    let route = match page {
        Some((skip, limit)) => {
            if order != ORDERS[0] && ORDERS.contains(&order) {
                parts.push(format!("order={order}"));
            }
            paging(&mut parts, skip, limit);
            "people.json"
        }
        None => "people/counts.json",
    };
    format!("/index/filter/{}/{route}{}", scope.segment(), query(parts))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The builders against atlas's own contract fixture: every url whose question can be asked through a tool is
    /// rebuilt from its parts, and must come out as atlas's canonical spelling.
    #[test]
    fn the_canonical_fixture_holds() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/facets-canonical.json")).unwrap();
        let mut checked = 0;
        for case in fixture["cases"].as_array().unwrap() {
            let url = case["url"].as_str().unwrap();
            let canonical = case["canonical"].as_str().unwrap();
            let (path, query) = url.split_once('?').unwrap_or((url, ""));
            let params: std::collections::HashMap<&str, String> = query
                .split('&')
                .filter_map(|p| p.split_once('='))
                .map(|(k, v)| (k, percent_decode(&v.replace('+', " "))))
                .collect();
            let segments: Vec<&str> = path.trim_start_matches("/index/filter/").split('/').collect();
            let scope = Scope::parse(segments[0]).unwrap();
            // A kind this server does not know is refused here where atlas would ignore it (`normalise`): the
            // fixture's `future:` case is checked for that and skipped.
            let unknown = ["sel", "traits"].iter().filter_map(|n| params.get(n)).any(|v| {
                v.split(',').filter_map(|i| i.trim_start_matches('-').split_once(':')).any(|(k, _)| {
                    let k = k.trim().to_ascii_lowercase();
                    !known_kind(&k) && k != "structure"
                })
            });
            if unknown {
                let sel = params.get("sel").unwrap();
                assert!(
                    items(std::slice::from_ref(sel), scope).is_err(),
                    "{url}: an unknown kind is refused"
                );
                continue;
            }
            let list = |name: &str| -> Vec<Item> {
                params.get(name).map(|v| items(std::slice::from_ref(v), scope).unwrap()).unwrap_or_default()
            };
            let number =
                |name: &str, default: usize| params.get(name).map_or(default, |v| v.parse().unwrap());
            let limit = number("limit", PAGE).clamp(1, MAX_PAGE);
            let order = params.get("order").map(|o| o.to_ascii_lowercase()).filter(|o| !o.is_empty());
            let order = order.as_deref().unwrap_or("prominence");
            let built = match &segments[1..] {
                ["counts.json"] => counts_url(scope, &list("sel")),
                ["titles.json"] => titles_url(scope, &list("sel"), number("skip", 0), limit),
                ["values", kind] => {
                    let kind = kind.trim_end_matches(".json");
                    let most = if kind == "character" { 5 } else { 10 };
                    values_url(
                        scope,
                        kind,
                        &list("sel"),
                        params.get("q").map(String::as_str),
                        number("limit", most),
                    )
                    .unwrap()
                }
                ["people.json"] => {
                    people_url(scope, &list("sel"), &list("traits"), order, Some((number("skip", 0), limit)))
                }
                ["people", "counts.json"] => {
                    people_url(scope, &list("sel"), &list("traits"), "prominence", None)
                }
                // Asked here without traits: `a_trait_lookup_is_spelled_as_atlas_answers_it` checks these.
                ["people", "values", _] => continue,
                other => panic!("an unknown route in the fixture: {other:?}"),
            };
            assert_eq!(built, canonical, "{url}");
            checked += 1;
        }
        assert!(checked >= 40, "the fixture lost its cases: {checked}");
        for refused in fixture["refused"].as_array().unwrap() {
            let url = refused.as_str().unwrap();
            let scope =
                Scope::parse(url.trim_start_matches("/index/filter/").split('/').next().unwrap()).unwrap();
            let query = url.split_once('?').map_or("", |(_, q)| q);
            let sel =
                query.split('&').find_map(|p| p.strip_prefix("sel=").or_else(|| p.strip_prefix("traits=")));
            let paged = query.contains("skip=10&limit=24") || query.contains("skip=x");
            let bad_order = query
                .split('&')
                .filter_map(|p| p.strip_prefix("order="))
                .any(|o| !ORDERS.contains(&o.to_ascii_lowercase().as_str()));
            let short_q = url.contains("/values/") && !url.contains("q=")
                || url.contains("q=b")
                || url.contains("q=wa")
                || query.split('&').any(|p| p == "q=i");
            if let Some(sel) = sel {
                // born:1970s and gender:male are refused by what the id is, like every other sel refusal.
                assert!(items(&[percent_decode(sel)], scope).is_err(), "{url} should be refused");
            } else {
                assert!(paged || short_q || bad_order, "{url}: a refusal this test does not model");
            }
        }
    }

    fn percent_decode(s: &str) -> String {
        let bytes = s.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
            }
            out.push(bytes[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// A kind is a path segment in `values/<kind>.json` and a prefix in `sel`: one built from what a caller sent could
    /// reach another route or carry a second parameter. Only a known kind of lower-case letters gets into a URL.
    #[test]
    fn a_kind_is_one_atlas_knows_or_nothing() {
        for kind in [
            "rating.json#",
            "character.json?q=walter%20white#",
            "../../../title/movie/1.json#",
            "x&sel=y",
            "",
        ] {
            assert_eq!(values_url(Scope::All, kind, &[], None, 10), None, "{kind}");
        }
        assert_eq!(values_url(Scope::All, "like", &[], None, 10), None, "like has no values");
        assert_eq!(values_url(Scope::All, "gender", &[], None, 10), None, "a trait is people/counts.json's");
        assert!(values_url(Scope::All, "person", &[], None, 10).is_some());
        for item in ["zz&sel=rating%3A8&yy:1", "zz:1", "Pers on:Q1", "person/../x:Q1", "future:X1"] {
            assert!(items(&[item.to_owned()], Scope::All).is_err(), "{item}");
        }
        assert_eq!(
            titles_url(Scope::All, &items(&["studio:q60".into()], Scope::All).unwrap(), 0, 24),
            "/index/filter/all/titles.json?sel=studio:Q60"
        );
    }

    #[test]
    fn a_people_order_follows_the_traits_and_the_default_is_left_out() {
        let traits = items(&["role:cast".into()], Scope::Movie).unwrap();
        assert_eq!(
            people_url(Scope::Movie, &[], &traits, "born_desc", Some((20, 20))),
            "/index/filter/movie/people.json?traits=role:cast&order=born_desc&skip=20&limit=20"
        );
        assert_eq!(
            people_url(Scope::Movie, &[], &traits, "prominence", Some((0, 24))),
            "/index/filter/movie/people.json?traits=role:cast"
        );
        assert_eq!(
            people_url(Scope::Movie, &[], &[], "name", None),
            "/index/filter/movie/people/counts.json"
        );
    }

    /// den-atlas#80's own canonical cases for its trait lookup (its `facets-canonical.json`), as this server asks
    /// them: it sends no `traits`, so the one case carrying them is checked without.
    #[test]
    fn a_trait_lookup_is_spelled_as_atlas_answers_it() {
        assert_eq!(
            people_values_url(Scope::Movie, &[], "citizenship", Some("Iceland"), 10).unwrap(),
            "/index/filter/movie/people/values/citizenship.json?q=iceland"
        );
        let drama = items(&["genre:18".into()], Scope::All).unwrap();
        assert_eq!(
            people_values_url(Scope::All, &drama, "occupation", Some("Film Dir"), 10).unwrap(),
            "/index/filter/all/people/values/occupation.json?sel=genre:18&q=film%20dir"
        );
        assert_eq!(
            people_values_url(Scope::Series, &[], "gender", None, 3).unwrap(),
            "/index/filter/series/people/values/gender.json?limit=3"
        );
        assert_eq!(
            people_values_url(Scope::Movie, &[], "citizenship", Some("i"), 10).unwrap(),
            "/index/filter/movie/people/values/citizenship.json",
            "a one-letter q, which atlas refuses, is not sent"
        );
        assert_eq!(
            people_values_url(Scope::All, &[], "born", None, 10),
            None,
            "born is listed whole elsewhere"
        );
    }

    #[test]
    fn a_page_is_a_whole_number_of_pages() {
        assert_eq!(page(2, 20), (40, 20));
        assert_eq!(page(0, 500), (0, 100));
        assert_eq!(page(3, 0), (3, 1));
    }

    #[test]
    fn a_country_or_language_may_be_named() {
        let spelled = |raw: &[&str]| {
            let raw: Vec<String> = raw.iter().map(|s| (*s).to_owned()).collect();
            titles_url(Scope::All, &items(&raw, Scope::All).unwrap(), 0, PAGE)
        };
        assert_eq!(
            spelled(&["language:Swedish", "country:Denmark"]),
            "/index/filter/all/titles.json?sel=country:DK,language:sv"
        );
        assert_eq!(
            spelled(&["country:swedish", "-language:English"]),
            spelled(&["country:SE", "-language:en"])
        );
        assert!(items(&["country:Narnia".into()], Scope::All).unwrap_err().contains("names no country"));
        assert!(items(&["language:Klingon".into()], Scope::All).is_err());
    }

    #[test]
    fn names_fold_as_atlas_looks_them_up() {
        assert_eq!(name_key("Bong Joon-ho"), "bong joon ho");
        assert_eq!(name_key("AMÉLIE"), "amelie");
        assert_eq!(name_key("Tom&Jerry"), "tom and jerry");
    }
}
