//! A projection (den-spec `wire/assistant-v1.md` §15) in memory: the compact form's lists as packed fixed-size rows
//! that index one table of titles. No `serde_json::Value`, no String per entry, so a library of tens of thousands of
//! plays takes about a megabyte and one pass to filter. It is the household's history, so it is wiped when dropped.
//!
//! A reader MUST ignore what it does not know (§15): members of an object it has not heard of, and elements past the
//! ones it reads in a row, so a later addition reaches this den-mcp harmlessly. A projection without `rated` or `hide`
//! (published before them) reads with none.

use serde::de::{self, Deserializer, IgnoredAny, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use std::fmt;
use zeroize::{DefaultIsZeroes, Zeroize};

/// A time or a number the entry does not have.
pub const NO_TIME: u64 = u64::MAX;
pub const NO_NUMBER: u32 = u32::MAX;
/// A title's kind is the top bit of its table entry; the rest is the TMDB id (at most 2^53 - 1).
const SERIES: u64 = 1 << 63;

/// A watchlist entry: when it was added.
#[derive(Clone, Copy, Default)]
pub struct Watch {
    pub at: u64,
    pub title: u32,
}

/// A Continue Watching entry.
#[derive(Clone, Copy, Default)]
pub struct Continue {
    pub at: u64,
    pub title: u32,
    pub season: u32,
    pub episode: u32,
    pub fraction: f32,
    /// `resume` the same film or episode; otherwise `next` is the episode after.
    pub resume: bool,
}

/// A Seen play.
#[derive(Clone, Copy, Default)]
pub struct Seen {
    pub at: u64,
    pub title: u32,
    pub season: u32,
    pub episode: u32,
}

/// A rating. Den's UI calls `Dislike` "Not for me".
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum Reaction {
    #[default]
    Dislike,
    Like,
    Love,
}

impl Reaction {
    pub fn name(self) -> &'static str {
        match self {
            Reaction::Dislike => "dislike",
            Reaction::Like => "like",
            Reaction::Love => "love",
        }
    }
}

/// A rated title.
#[derive(Clone, Copy, Default)]
pub struct Rated {
    pub at: u64,
    pub title: u32,
    pub reaction: Reaction,
}

impl DefaultIsZeroes for Watch {}
impl DefaultIsZeroes for Continue {}
impl DefaultIsZeroes for Seen {}
impl DefaultIsZeroes for Rated {}

/// The household's hide rules, as Den Web sends them to `/recommend` (`hide`).
#[derive(Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Hide {
    pub min_year: Option<i64>,
    /// TMDB genre ids.
    pub genres: Vec<u16>,
    /// ISO 639-1 original languages.
    pub languages: Vec<String>,
    pub anime: bool,
}

/// The next element of a row, or the row is too short.
fn field<'de, A: SeqAccess<'de>, T: Deserialize<'de>>(seq: &mut A, at: usize) -> Result<T, A::Error> {
    seq.next_element()?.ok_or_else(|| de::Error::invalid_length(at, &"a longer row"))
}

/// Whatever follows the elements a row is read for: a later addition to the row, ignored.
fn rest<'de, A: SeqAccess<'de>>(seq: &mut A) -> Result<(), A::Error> {
    while seq.next_element::<IgnoredAny>()?.is_some() {}
    Ok(())
}

/// A row type read from a JSON array: `$build` reads its elements in order from `$seq`, and what is left is skipped.
macro_rules! row {
    ($ty:ty, $expecting:literal, |$seq:ident| $build:expr) => {
        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct V;
                impl<'de> Visitor<'de> for V {
                    type Value = $ty;
                    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                        f.write_str($expecting)
                    }
                    fn visit_seq<A: SeqAccess<'de>>(self, mut $seq: A) -> Result<$ty, A::Error> {
                        let row = $build;
                        rest(&mut $seq)?;
                        Ok(row)
                    }
                }
                d.deserialize_seq(V)
            }
        }
    };
}

/// One title of the table, as the compact form writes it: `["movie" | "tv", <id>]`.
struct Title(u64);

row!(Title, "a title", |seq| {
    let kind: &str = field(&mut seq, 0)?;
    let id: u64 = field(&mut seq, 1)?;
    let kind = match kind {
        "movie" => 0,
        "tv" => SERIES,
        _ => return Err(de::Error::custom("unknown kind")),
    };
    if id == 0 || id > 9_007_199_254_740_991 {
        return Err(de::Error::custom("id out of range"));
    }
    Title(kind | id)
});

fn at(time: Option<u64>) -> u64 {
    time.unwrap_or(NO_TIME)
}

/// A season or an episode, or none (u32::MAX is "none", so it is not a number).
fn place<E: de::Error>(n: Option<u32>) -> Result<u32, E> {
    match n {
        Some(NO_NUMBER) => Err(E::custom("number out of range")),
        Some(n) => Ok(n),
        None => Ok(NO_NUMBER),
    }
}

row!(Watch, "a watchlist row", |seq| {
    let title: u32 = field(&mut seq, 0)?;
    let added: Option<u64> = field(&mut seq, 1)?;
    Watch { at: at(added), title }
});

row!(Continue, "a Continue Watching row", |seq| {
    let title: u32 = field(&mut seq, 0)?;
    let action: &str = field(&mut seq, 1)?;
    let season: Option<u32> = field(&mut seq, 2)?;
    let episode: Option<u32> = field(&mut seq, 3)?;
    let fraction: f32 = field(&mut seq, 4)?;
    let time: Option<u64> = field(&mut seq, 5)?;
    let resume = match action {
        "resume" => true,
        "next" => false,
        _ => return Err(de::Error::custom("unknown action")),
    };
    let (season, episode) = (place::<A::Error>(season)?, place::<A::Error>(episode)?);
    Continue { at: at(time), title, season, episode, fraction, resume }
});

row!(Seen, "a Seen row", |seq| {
    let title: u32 = field(&mut seq, 0)?;
    let season: Option<u32> = field(&mut seq, 1)?;
    let episode: Option<u32> = field(&mut seq, 2)?;
    let time: Option<u64> = field(&mut seq, 3)?;
    let (season, episode) = (place::<A::Error>(season)?, place::<A::Error>(episode)?);
    Seen { at: at(time), title, season, episode }
});

row!(Rated, "a rating row", |seq| {
    let title: u32 = field(&mut seq, 0)?;
    let value: &str = field(&mut seq, 1)?;
    let time: Option<u64> = field(&mut seq, 2)?;
    let reaction = match value {
        "dislike" => Reaction::Dislike,
        "like" => Reaction::Like,
        "love" => Reaction::Love,
        _ => return Err(de::Error::custom("unknown rating")),
    };
    Rated { at: at(time), title, reaction }
});

#[derive(Deserialize)]
struct Wire {
    at: u64,
    titles: Vec<Title>,
    watchlist: Vec<Watch>,
    #[serde(rename = "continue")]
    continuing: Vec<Continue>,
    seen: Vec<Seen>,
    #[serde(default)]
    rated: Vec<Rated>,
    #[serde(default)]
    hide: Hide,
}

/// The lists of a projection, packed.
pub struct Lists {
    /// When a device published them, Unix ms.
    pub at: u64,
    titles: Vec<u64>,
    pub watchlist: Vec<Watch>,
    pub continuing: Vec<Continue>,
    pub seen: Vec<Seen>,
    pub rated: Vec<Rated>,
    pub hide: Hide,
}

impl Lists {
    /// The compact form's JCS bytes read into packed rows. `None` for anything that is not one — an entry naming a
    /// title the table lacks included.
    pub fn parse(compact: &[u8]) -> Option<Lists> {
        let wire: Wire = serde_json::from_slice(compact).ok()?;
        let table = u32::try_from(wire.titles.len()).ok()?;
        let named = wire.watchlist.iter().all(|e| e.title < table)
            && wire.continuing.iter().all(|e| e.title < table)
            && wire.seen.iter().all(|e| e.title < table)
            && wire.rated.iter().all(|e| e.title < table);
        named.then(|| Lists {
            at: wire.at,
            titles: wire.titles.iter().map(|t| t.0).collect(),
            watchlist: wire.watchlist,
            continuing: wire.continuing,
            seen: wire.seen,
            rated: wire.rated,
            hide: wire.hide,
        })
    }

    /// A title of the table: Den's kind (`series` is the library's `tv`) and TMDB id.
    pub fn title(&self, index: u32) -> (&'static str, u64) {
        let t = self.titles[index as usize];
        (if t & SERIES != 0 { "series" } else { "movie" }, t & !SERIES)
    }

    /// The memory the rows take.
    pub fn bytes(&self) -> usize {
        self.titles.len() * 8
            + self.watchlist.len() * size_of::<Watch>()
            + self.continuing.len() * size_of::<Continue>()
            + self.seen.len() * size_of::<Seen>()
            + self.rated.len() * size_of::<Rated>()
    }
}

impl Drop for Lists {
    fn drop(&mut self) {
        self.titles.zeroize();
        self.watchlist.zeroize();
        self.continuing.zeroize();
        self.seen.zeroize();
        self.rated.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_compact_form_reads_into_packed_rows() {
        let lists = Lists::parse(
            br#"{"v":1,"library":"l","grant":"g","at":5,"head":9,"titles":[["movie",550],["tv",1399]],
                "watchlist":[[0,100],[1,null]],
                "continue":[[1,"resume",1,2,0.5,70],[0,"next",null,null,0.0,null]],
                "seen":[[1,1,1,50],[0,null,null,null]],
                "rated":[[0,"love",60],[1,"dislike",null]],
                "hide":{"minYear":1990,"genres":[16,27],"languages":["tr"],"anime":true}}"#,
        )
        .unwrap();
        assert_eq!(lists.at, 5);
        assert_eq!((lists.title(0), lists.title(1)), (("movie", 550), ("series", 1399)));
        assert_eq!((lists.watchlist[0].at, lists.watchlist[1].at), (100, NO_TIME));
        let c = lists.continuing[0];
        assert_eq!((c.title, c.season, c.episode, c.fraction, c.at, c.resume), (1, 1, 2, 0.5, 70, true));
        assert_eq!((lists.continuing[1].season, lists.continuing[1].resume), (NO_NUMBER, false));
        assert_eq!((lists.seen[1].at, lists.seen[1].episode), (NO_TIME, NO_NUMBER));
        assert_eq!((lists.rated[0].reaction, lists.rated[0].at), (Reaction::Love, 60));
        assert_eq!((lists.rated[1].reaction, lists.rated[1].at), (Reaction::Dislike, NO_TIME));
        assert_eq!(lists.hide.min_year, Some(1990));
        assert_eq!(lists.hide.genres, [16, 27]);
        assert_eq!(lists.hide.languages, ["tr"]);
        assert!(lists.hide.anime);
        assert!(lists.bytes() > 0);
    }

    #[test]
    fn a_projection_from_before_ratings_and_hide_reads_with_none() {
        let lists = Lists::parse(
            br#"{"at":1,"titles":[["movie",1]],"watchlist":[[0,null]],"continue":[],"seen":[]}"#,
        )
        .unwrap();
        assert!(lists.rated.is_empty());
        assert_eq!((lists.hide.min_year, lists.hide.genres.len(), lists.hide.anime), (None, 0, false));
        // A hide with its members null or missing is no rule.
        let lists = Lists::parse(
            br#"{"at":1,"titles":[],"watchlist":[],"continue":[],"seen":[],"hide":{"minYear":null,"anime":false}}"#,
        )
        .unwrap();
        assert!(lists.hide.min_year.is_none() && lists.hide.languages.is_empty());
    }

    #[test]
    fn what_a_later_spec_adds_is_ignored() {
        // Members this den-mcp does not know — at the top, in `hide`, and past a row's last element — change nothing.
        let lists = Lists::parse(
            br#"{"v":1,"at":5,"head":9,"future":{"a":[1,2]},"extra":"x",
                "titles":[["movie",550,"future"],["tv",1399]],
                "watchlist":[[0,100,"future",7]],
                "continue":[[1,"resume",1,2,0.5,70,{"x":1}]],
                "seen":[[1,1,1,50,null]],
                "rated":[[0,"like",60,"future"]],
                "hide":{"minYear":1990,"genres":[],"languages":[],"anime":false,"future":true},
                "later":[]}"#,
        )
        .expect("unknown members are ignored");
        assert_eq!(lists.title(0), ("movie", 550));
        assert_eq!((lists.watchlist.len(), lists.continuing.len(), lists.seen.len()), (1, 1, 1));
        assert_eq!((lists.rated[0].reaction, lists.hide.min_year), (Reaction::Like, Some(1990)));
    }

    #[test]
    fn what_is_not_a_compact_projection_is_refused() {
        let with = |titles: &str, rows: &str| {
            format!(r#"{{"at":1,"titles":{titles},"watchlist":{rows},"continue":[],"seen":[]}}"#)
        };
        assert!(Lists::parse(with(r#"[["movie",1]]"#, "[[0,null]]").as_bytes()).is_some());
        for (titles, rows, why) in [
            (r#"[["movie",1]]"#, "[[1,null]]", "an index past the table"),
            (r#"[["film",1]]"#, "[]", "an unknown kind"),
            (r#"[["movie",0]]"#, "[]", "an id of zero"),
            (r#"[["movie",9007199254740992]]"#, "[]", "an id past 2^53 - 1"),
            (r#"[["movie",1]]"#, "[[0]]", "a short row"),
        ] {
            assert!(Lists::parse(with(titles, rows).as_bytes()).is_none(), "{why}");
        }
        assert!(Lists::parse(b"[]").is_none());
        let rated = |row: &str| {
            format!(
                r#"{{"at":1,"titles":[["movie",1]],"watchlist":[],"continue":[],"seen":[],"rated":[{row}]}}"#
            )
        };
        assert!(Lists::parse(rated(r#"[0,"like",null]"#).as_bytes()).is_some());
        assert!(Lists::parse(rated(r#"[0,"meh",null]"#).as_bytes()).is_none(), "an unknown rating");
        assert!(Lists::parse(rated(r#"[1,"like",null]"#).as_bytes()).is_none(), "an index past the table");
    }
}
