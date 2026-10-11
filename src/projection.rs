//! A projection (den-spec `wire/assistant-v1.md` §15) in memory: the compact form's lists as packed fixed-size rows
//! that index one table of titles. No `serde_json::Value`, no String per entry, so a library of tens of thousands of
//! plays takes about a megabyte and one pass to filter. It is the household's history, so it is wiped when dropped.

use serde::{Deserialize, Deserializer};
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

impl DefaultIsZeroes for Watch {}
impl DefaultIsZeroes for Continue {}
impl DefaultIsZeroes for Seen {}

/// One title of the table, as the compact form writes it: `["movie" | "tv", <id>]`.
struct Title(u64);

fn number<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    // u32::MAX is "none": a number that large is not a season or an episode.
    u32::deserialize(d).and_then(|n| {
        if n == NO_NUMBER {
            Err(serde::de::Error::custom("number out of range"))
        } else {
            Ok(n)
        }
    })
}

#[derive(Deserialize)]
struct Number(#[serde(deserialize_with = "number")] u32);

impl<'de> Deserialize<'de> for Title {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let (kind, id): (&str, u64) = Deserialize::deserialize(d)?;
        let kind = match kind {
            "movie" => 0,
            "tv" => SERIES,
            _ => return Err(serde::de::Error::custom("unknown kind")),
        };
        if id == 0 || id > 9_007_199_254_740_991 {
            return Err(serde::de::Error::custom("id out of range"));
        }
        Ok(Title(kind | id))
    }
}

fn at(time: Option<u64>) -> u64 {
    time.unwrap_or(NO_TIME)
}

fn place(n: Option<Number>) -> u32 {
    n.map_or(NO_NUMBER, |n| n.0)
}

impl<'de> Deserialize<'de> for Watch {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let (title, added): (u32, Option<u64>) = Deserialize::deserialize(d)?;
        Ok(Watch { at: at(added), title })
    }
}

impl<'de> Deserialize<'de> for Continue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let (title, action, season, episode, fraction, time): (
            u32,
            &str,
            Option<Number>,
            Option<Number>,
            f32,
            Option<u64>,
        ) = Deserialize::deserialize(d)?;
        let resume = match action {
            "resume" => true,
            "next" => false,
            _ => return Err(serde::de::Error::custom("unknown action")),
        };
        Ok(Continue { at: at(time), title, season: place(season), episode: place(episode), fraction, resume })
    }
}

impl<'de> Deserialize<'de> for Seen {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let (title, season, episode, time): (u32, Option<Number>, Option<Number>, Option<u64>) =
            Deserialize::deserialize(d)?;
        Ok(Seen { at: at(time), title, season: place(season), episode: place(episode) })
    }
}

#[derive(Deserialize)]
struct Wire {
    at: u64,
    titles: Vec<Title>,
    watchlist: Vec<Watch>,
    #[serde(rename = "continue")]
    continuing: Vec<Continue>,
    seen: Vec<Seen>,
}

/// The lists of a projection, packed.
pub struct Lists {
    /// When a device published them, Unix ms.
    pub at: u64,
    titles: Vec<u64>,
    pub watchlist: Vec<Watch>,
    pub continuing: Vec<Continue>,
    pub seen: Vec<Seen>,
}

impl Lists {
    /// The compact form's JCS bytes read into packed rows. `None` for anything that is not one — an entry naming a
    /// title the table lacks included.
    pub fn parse(compact: &[u8]) -> Option<Lists> {
        let wire: Wire = serde_json::from_slice(compact).ok()?;
        let table = u32::try_from(wire.titles.len()).ok()?;
        let named = wire.watchlist.iter().all(|e| e.title < table)
            && wire.continuing.iter().all(|e| e.title < table)
            && wire.seen.iter().all(|e| e.title < table);
        named.then(|| Lists {
            at: wire.at,
            titles: wire.titles.iter().map(|t| t.0).collect(),
            watchlist: wire.watchlist,
            continuing: wire.continuing,
            seen: wire.seen,
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
    }
}

impl Drop for Lists {
    fn drop(&mut self) {
        self.titles.zeroize();
        self.watchlist.zeroize();
        self.continuing.zeroize();
        self.seen.zeroize();
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
                "seen":[[1,1,1,50],[0,null,null,null]]}"#,
        )
        .unwrap();
        assert_eq!(lists.at, 5);
        assert_eq!((lists.title(0), lists.title(1)), (("movie", 550), ("series", 1399)));
        assert_eq!((lists.watchlist[0].at, lists.watchlist[1].at), (100, NO_TIME));
        let c = lists.continuing[0];
        assert_eq!((c.title, c.season, c.episode, c.fraction, c.at, c.resume), (1, 1, 2, 0.5, 70, true));
        assert_eq!((lists.continuing[1].season, lists.continuing[1].resume), (NO_NUMBER, false));
        assert_eq!((lists.seen[1].at, lists.seen[1].episode), (NO_TIME, NO_NUMBER));
        assert!(lists.bytes() > 0);
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
    }
}
