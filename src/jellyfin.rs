//! Jellyfin search for "play ... on Jellyfin".
//!
//! The server finds a series or a movie in the Jellyfin library and returns
//! the item to play. Home Assistant then opens it in an app on the TV box. The
//! rules:
//!
//! 1. A series: the next episode of the user ("next up"), with its resume
//!    position. If the user watched all episodes, the first episode. A season
//!    and an episode number pick that episode.
//! 2. A movie: the movie. Of several movies that match equally well (a film
//!    series), the oldest one that the user has not watched.
//!
//! Jellyfin's own search needs the words of the title in order, so "spy
//! family" does not find "Spy x Family". The server therefore keeps the list
//! of all series and movie titles (refreshed every 10 min) and searches in
//! stages ([`Match`]): the title, then trigram similarity for typos, then the
//! optional media index (descriptions, mishearings), then Jellyfin's own
//! search for items newer than the list. Each later stage needs a minimum
//! similarity, because a wrong movie on the TV is worse than no answer.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::config::JellyfinConfig;
use crate::text;

const LIBRARY_TTL: Duration = Duration::from_secs(600);
/// Trigram similarity from which a title counts as a typo of the query.
/// "lord of the rinks" has 0.6 with "The Lord of the Rings", "the office"
/// only 0.4 with "Office Space".
const FUZZY_MIN: f32 = 0.5;
/// Words that people say or that STT writes between title words: "spy by
/// family" for "Spy x Family". They are ignored in the query.
const FILLER: &[&str] = &["the", "a", "an", "x", "by", "and", "ex", "of", "on", "jellyfin"];

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Item {
    pub id: String,
    /// Jellyfin sends episodes without a name now and then.
    #[serde(default)]
    pub name: String,
    #[serde(rename = "Type")]
    pub kind: String,
    #[serde(default)]
    pub production_year: Option<i32>,
    #[serde(default)]
    pub user_data: UserData,
    #[serde(default)]
    pub parent_index_number: Option<u32>,
    #[serde(default)]
    pub index_number: Option<u32>,
    #[serde(default)]
    pub series_id: Option<String>,
    #[serde(default)]
    pub series_name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserData {
    #[serde(default)]
    pub played: bool,
    #[serde(default)]
    pub playback_position_ticks: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Items {
    items: Vec<Item>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Episode,
    Movie,
}

/// Which stage of the search found the item.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Match {
    /// The title, its main part, or all query words in the title.
    Title,
    /// Trigram similarity of the title: typos and small mishearings.
    Fuzzy,
    /// The consonants of the title: misheard vowels ("freeran" for "Frieren").
    Sound,
    /// The media index (EmbeddingGemma 2): descriptions, worse mishearings.
    Semantic,
    /// Jellyfin's own search: items newer than the cached title list.
    Jellyfin,
}

#[derive(Debug, Clone, Serialize)]
pub struct Pick {
    /// The stage that found it, and its score (1 for a title match).
    #[serde(rename = "match")]
    pub matched: Match,
    pub score: f32,
    pub kind: Kind,
    pub query: String,
    pub item_id: String,
    /// The series or the movie.
    pub title: String,
    /// The episode title, for an episode.
    pub episode_title: Option<String>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    /// Where the user stopped, in seconds. 0 starts at the beginning.
    pub resume_seconds: u64,
    pub user: String,
    /// User and server, so that an app can switch to the right account.
    pub user_id: String,
    pub server_id: String,
}

/// How well a title matches the query. Lower is better.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Score {
    /// The whole title.
    Exact,
    /// The title before a colon: "Frieren" for "Frieren: Beyond Journey's End".
    Main,
    /// All query words are words of the title.
    Words,
}

fn score(query: &str, title: &str) -> Option<Score> {
    let q = text::squashed(query);
    if q.is_empty() {
        return None;
    }
    let main = title.split(':').next().unwrap_or(title);
    if q == text::squashed(title) {
        return Some(Score::Exact);
    }
    if q == text::squashed(main) {
        return Some(Score::Main);
    }
    let wanted: Vec<String> = text::words(query)
        .into_iter()
        .filter(|w| !FILLER.contains(&w.as_str()))
        .collect();
    // All query words must be in the title (or its main part), and they must be
    // more than half of its words: "the office" must not find "Office Space".
    let covers = |t: &str| {
        let have: Vec<String> = text::words(t)
            .into_iter()
            .filter(|w| !FILLER.contains(&w.as_str()))
            .collect();
        wanted.iter().all(|w| have.contains(w)) && wanted.len() * 2 > have.len()
    };
    (!wanted.is_empty() && (covers(title) || covers(main))).then_some(Score::Words)
}

/// The best series or movie for the query. Series win over movies at the same
/// score. Of several movies, the oldest unwatched one wins.
pub fn best_match<'a>(query: &str, library: &'a [Item]) -> Option<&'a Item> {
    library
        .iter()
        .filter_map(|item| score(query, &item.name).map(|s| (s, item)))
        .min_by_key(|(s, item)| {
            (
                *s,
                item.kind != "Series",
                item.user_data.played,
                item.production_year.unwrap_or(i32::MAX),
            )
        })
        .map(|(_, item)| item)
}

/// The title before a colon: "Frieren" for "Frieren: Beyond Journey's End".
fn main_title(title: &str) -> &str {
    title.split(':').next().unwrap_or(title)
}

/// The series or movie whose title (or its main part) is most similar to the
/// query, if the similarity reaches [`FUZZY_MIN`]. Ties go to series, then to
/// the oldest unwatched movie, as in [`best_match`].
pub fn fuzzy_match<'a>(query: &str, library: &'a [Item]) -> Option<(&'a Item, f32)> {
    let q = text::trigrams(query);
    library
        .iter()
        .map(|item| {
            let full = text::trigram_similarity(&q, &text::trigrams(&item.name));
            let main = text::trigram_similarity(&q, &text::trigrams(main_title(&item.name)));
            (item, full.max(main))
        })
        .filter(|(_, sim)| *sim >= FUZZY_MIN)
        .min_by(|(a, sa), (b, sb)| {
            sb.total_cmp(sa).then_with(|| {
                (
                    a.kind != "Series",
                    a.user_data.played,
                    a.production_year.unwrap_or(i32::MAX),
                )
                    .cmp(&(
                        b.kind != "Series",
                        b.user_data.played,
                        b.production_year.unwrap_or(i32::MAX),
                    ))
            })
        })
}

/// The one series or movie whose main title has the same consonants as the
/// query ([`text::skeleton`], at least 3). Several titles with the same
/// consonants count as one only if they share the main title (a film series).
pub fn sound_match<'a>(query: &str, library: &'a [Item]) -> Option<&'a Item> {
    let q = text::skeleton(query);
    if q.len() < 3 {
        return None;
    }
    let found: Vec<&Item> = library
        .iter()
        .filter(|i| text::skeleton(main_title(&i.name)) == q)
        .collect();
    let first = *found.first()?;
    let same_work = found
        .iter()
        .all(|i| text::squashed(main_title(&i.name)) == text::squashed(main_title(&first.name)));
    if !same_work {
        return None;
    }
    found.into_iter().min_by_key(|i| {
        (
            i.kind != "Series",
            i.user_data.played,
            i.production_year.unwrap_or(i32::MAX),
        )
    })
}

/// For a title-like query, the semantic winner must share letters with it:
/// otherwise the model picks a related work ("House of the Dragon" for "game
/// of thrones", "Futurama" for "fryren"). Descriptions need no such check.
const SEMANTIC_TITLE_MIN: f32 = 0.2;

pub fn semantic_title_ok(query: &str, title: &str) -> bool {
    if !text::title_like(query) {
        return true;
    }
    let sim = text::similarity(query, title).max(text::similarity(query, main_title(title)));
    sim >= SEMANTIC_TITLE_MIN || text::skeleton(main_title(title)).starts_with(&text::skeleton(query))
}

/// "Spy x Family season 2 episode 3" becomes ("Spy x Family", Some(2), Some(3)).
pub fn split_episode(query: &str) -> (String, Option<u32>, Option<u32>) {
    let words: Vec<String> = query.split_whitespace().map(str::to_owned).collect();
    let lower: Vec<String> = text::words(query);
    if lower.len() != words.len() {
        return (query.trim().to_owned(), None, None);
    }
    let number_after = |key: &str| -> Option<(usize, u32)> {
        let i = lower.iter().position(|w| w == key)?;
        lower.get(i + 1)?.parse().ok().map(|n| (i, n))
    };
    let season = number_after("season");
    let episode = number_after("episode");
    let cut = [season.map(|(i, _)| i), episode.map(|(i, _)| i)]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(words.len());
    (words[..cut].join(" "), season.map(|(_, n)| n), episode.map(|(_, n)| n))
}

pub struct Jellyfin {
    /// The optional media index (MEDIA_INDEX=on), attached after start.
    media: std::sync::OnceLock<std::sync::Arc<crate::media::MediaIndex>>,
    client: reqwest::Client,
    cfg: JellyfinConfig,
    /// (user ID, fetched at, items) per user.
    library: Mutex<Option<(String, Instant, Vec<Item>)>>,
}

impl Jellyfin {
    pub fn new(cfg: JellyfinConfig) -> Result<Self> {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(15)).build()?;
        Ok(Self {
            media: std::sync::OnceLock::new(),
            client,
            cfg,
            library: Mutex::new(None),
        })
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<T> {
        let resp = self
            .client
            .get(format!("{}{path}", self.cfg.url.trim_end_matches('/')))
            .header("Authorization", format!("MediaBrowser Token=\"{}\"", self.cfg.api_key))
            .query(query)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            bail!("Jellyfin {path}: HTTP {status}");
        }
        resp.json().await.with_context(|| format!("Jellyfin {path}: bad JSON"))
    }

    /// One page of library items as raw JSON, and the total count. `ids`
    /// limits the result to these items. Collections are expanded, missing
    /// (virtual) episodes are left out.
    pub async fn items_page(
        &self,
        types: &str,
        fields: &str,
        start: usize,
        limit: usize,
        ids: Option<&str>,
    ) -> Result<(Vec<Value>, usize)> {
        let start = start.to_string();
        let limit = limit.to_string();
        let mut query = vec![
            ("IncludeItemTypes", types),
            ("Recursive", "true"),
            ("Fields", fields),
            ("StartIndex", start.as_str()),
            ("Limit", limit.as_str()),
            ("EnableImages", "false"),
            ("EnableUserData", "false"),
            ("CollapseBoxSetItems", "false"),
            ("IsMissing", "false"),
            ("EnableTotalRecordCount", "true"),
        ];
        if let Some(ids) = ids {
            query.push(("Ids", ids));
        }
        let page: Value = self.get("/Items", &query).await?;
        let items = page["Items"].as_array().cloned().unwrap_or_default();
        let total = page["TotalRecordCount"].as_u64().unwrap_or(0) as usize;
        Ok((items, total))
    }

    /// Uses the media index for the semantic stage of the search.
    pub fn attach_media(&self, media: std::sync::Arc<crate::media::MediaIndex>) {
        let _ = self.media.set(media);
    }

    /// One item with the data of a user.
    async fn item(&self, user_id: &str, id: &str) -> Result<Item> {
        self.get(&format!("/Items/{id}"), &[("userId", user_id)]).await
    }

    /// The best series, movie or episode for the spoken words: title, then
    /// trigram similarity, then the media index, then Jellyfin's own search.
    async fn find(&self, user_id: &str, words: &str, library: &[Item]) -> Result<Option<(Item, Match, f32)>> {
        if let Some(item) = best_match(words, library) {
            return Ok(Some((item.clone(), Match::Title, 1.0)));
        }
        if let Some((item, sim)) = fuzzy_match(words, library) {
            tracing::info!("jellyfin {words:?}: fuzzy title match {} ({sim:.2})", item.name);
            return Ok(Some((item.clone(), Match::Fuzzy, sim)));
        }
        if let Some(item) = sound_match(words, library) {
            tracing::info!("jellyfin {words:?}: sound match {}", item.name);
            return Ok(Some((item.clone(), Match::Sound, 0.5)));
        }
        if let Some(media) = self.media.get().filter(|m| m.ready()) {
            match media.semantic(words).await {
                Ok(Some(s)) => {
                    let lexical = semantic_title_ok(words, &s.name);
                    let verdict = match (s.accepted, lexical) {
                        (true, true) => "accepted",
                        (true, false) => "rejected, the title shares too few letters with the query",
                        (false, _) => "rejected by the gate",
                    };
                    tracing::info!(
                        "jellyfin {words:?}: semantic {} ({:?}) score {:.3} margin {:.3}: {verdict}",
                        s.name,
                        s.kind,
                        s.score,
                        s.margin
                    );
                    if s.accepted && lexical {
                        let item = match library.iter().find(|i| i.id == s.id) {
                            Some(i) => i.clone(),
                            None => self.item(user_id, &s.id).await?,
                        };
                        return Ok(Some((item, Match::Semantic, s.score)));
                    }
                }
                Ok(None) => {}
                Err(err) => tracing::warn!("jellyfin {words:?}: media index: {err:#}"),
            }
        }
        // Items added after the title list was cached. Only a close title counts:
        // Jellyfin's search finds "Office Space" for "the office".
        let found: Items = self
            .get(
                "/Items",
                &[
                    ("searchTerm", words),
                    ("IncludeItemTypes", "Series,Movie"),
                    ("Recursive", "true"),
                    ("userId", user_id),
                    ("Limit", "1"),
                ],
            )
            .await?;
        if let Some(item) = found.items.into_iter().next() {
            let sim = text::similarity(words, main_title(&item.name)).max(text::similarity(words, &item.name));
            if sim >= FUZZY_MIN {
                tracing::info!("jellyfin {words:?}: Jellyfin search {} ({sim:.2})", item.name);
                return Ok(Some((item, Match::Jellyfin, sim)));
            }
        }
        Ok(None)
    }

    /// The base URL and the API key, for the WebSocket of the media index.
    pub fn server(&self) -> (&str, &str) {
        (self.cfg.url.trim_end_matches('/'), &self.cfg.api_key)
    }

    /// The ID of a user name (case does not matter).
    async fn user_id(&self, name: &str) -> Result<String> {
        let users: Vec<Value> = self.get("/Users", &[]).await?;
        users
            .iter()
            .find(|u| u["Name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(name)))
            .and_then(|u| u["Id"].as_str())
            .map(str::to_owned)
            .with_context(|| format!("no Jellyfin user {name}"))
    }

    async fn library(&self, user_id: &str) -> Result<Vec<Item>> {
        let mut cache = self.library.lock().await;
        if let Some((uid, at, items)) = cache.as_ref()
            && uid == user_id
            && at.elapsed() < LIBRARY_TTL
        {
            return Ok(items.clone());
        }
        let items: Items = self
            .get(
                "/Items",
                &[
                    ("IncludeItemTypes", "Series,Movie"),
                    ("Recursive", "true"),
                    ("userId", user_id),
                    ("Fields", "ProductionYear"),
                    ("EnableImages", "false"),
                    ("EnableUserData", "true"),
                ],
            )
            .await?;
        tracing::info!("jellyfin: {} series and movies", items.items.len());
        *cache = Some((user_id.to_owned(), Instant::now(), items.items.clone()));
        Ok(items.items)
    }

    async fn episodes(&self, user_id: &str, series_id: &str) -> Result<Vec<Item>> {
        let path = format!("/Shows/{series_id}/Episodes");
        let list: Items = self.get(&path, &[("userId", user_id)]).await?;
        Ok(list.items)
    }

    async fn next_up(&self, user_id: &str, series_id: &str) -> Result<Option<Item>> {
        let list: Items = self
            .get(
                "/Shows/NextUp",
                &[
                    ("seriesId", series_id),
                    ("userId", user_id),
                    ("Limit", "1"),
                    ("enableResumable", "true"),
                ],
            )
            .await?;
        Ok(list.items.into_iter().next())
    }

    /// The user of a Jellyfin session on one of `devices` (device names,
    /// comma-separated), active in the last 15 min. A session that plays
    /// something wins over an idle one.
    async fn session_user(&self, devices: &str) -> Result<Option<String>> {
        let sessions: Vec<Value> = self.get("/Sessions", &[("activeWithinSeconds", "900")]).await?;
        Ok(session_user_of(&sessions, devices))
    }

    /// The item to play for a spoken query. The user: `user` if given, else
    /// the user of an open session on one of `devices`, else `JELLYFIN_USER`.
    pub async fn search(&self, query: &str, user: Option<&str>, devices: Option<&str>) -> Result<Option<Pick>> {
        let from_session = match (user.filter(|u| !u.is_empty()), devices.filter(|d| !d.is_empty())) {
            (None, Some(devices)) => self.session_user(devices).await.unwrap_or_else(|err| {
                tracing::warn!("jellyfin sessions: {err:#}");
                None
            }),
            _ => None,
        };
        let user = user
            .filter(|u| !u.is_empty())
            .map(str::to_owned)
            .or(from_session)
            .unwrap_or_else(|| self.cfg.user.clone());
        let user_id = self.user_id(&user).await?;
        let info: Value = self.get("/System/Info/Public", &[]).await?;
        let server_id = info["Id"].as_str().unwrap_or_default().to_owned();
        let (words, season, episode) = split_episode(query);
        if words.is_empty() {
            return Ok(None);
        }
        let library = self.library(&user_id).await?;
        let Some((mut item, matched, score)) = self.find(&user_id, &words, &library).await? else {
            return Ok(None);
        };
        // An episode from the media index: with a season or episode number, its
        // series decides. Otherwise that episode plays.
        if item.kind == "Episode"
            && (season.is_some() || episode.is_some())
            && let Some(series) = item
                .series_id
                .as_deref()
                .and_then(|id| library.iter().find(|i| i.id == id))
        {
            item = series.clone();
        }
        let title = if item.kind == "Episode" {
            item.series_name.clone().unwrap_or_else(|| item.name.clone())
        } else {
            item.name.clone()
        };
        let pick = |ep: Option<&Item>, target: &Item| Pick {
            matched,
            score,
            kind: if ep.is_some() { Kind::Episode } else { Kind::Movie },
            query: words.clone(),
            item_id: target.id.clone(),
            title: title.clone(),
            episode_title: ep.map(|e| e.name.clone()),
            season: ep.and_then(|e| e.parent_index_number),
            episode: ep.and_then(|e| e.index_number),
            resume_seconds: if target.user_data.played {
                0
            } else {
                target.user_data.playback_position_ticks / 10_000_000
            },
            user: user.clone(),
            user_id: user_id.clone(),
            server_id: server_id.clone(),
        };
        if item.kind == "Episode" {
            return Ok(Some(pick(Some(&item), &item)));
        }
        if item.kind != "Series" {
            return Ok(Some(pick(None, &item)));
        }
        let chosen = if season.is_some() || episode.is_some() {
            self.episodes(&user_id, &item.id).await?.into_iter().find(|e| {
                season.is_none_or(|s| e.parent_index_number == Some(s)) && e.index_number == Some(episode.unwrap_or(1))
            })
        } else {
            match self.next_up(&user_id, &item.id).await? {
                Some(e) => Some(e),
                // All watched (or nothing started): the first episode.
                None => self.episodes(&user_id, &item.id).await?.into_iter().next(),
            }
        };
        Ok(chosen.map(|e| pick(Some(&e), &e)))
    }
}

/// See `Jellyfin::session_user`. Sessions come newest first from Jellyfin.
pub fn session_user_of(sessions: &[Value], devices: &str) -> Option<String> {
    let wanted: Vec<String> = devices.split(',').map(|d| d.trim().to_lowercase()).collect();
    let on_device: Vec<&Value> = sessions
        .iter()
        .filter(|s| {
            s["DeviceName"]
                .as_str()
                .is_some_and(|d| wanted.contains(&d.to_lowercase()))
                && s["UserName"].is_string()
        })
        .collect();
    on_device
        .iter()
        .find(|s| s["NowPlayingItem"].is_object())
        .or(on_device.first())
        .and_then(|s| s["UserName"].as_str())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, kind: &str, year: i32, played: bool) -> Item {
        Item {
            id: name.to_owned(),
            name: name.to_owned(),
            kind: kind.to_owned(),
            production_year: Some(year),
            user_data: UserData {
                played,
                playback_position_ticks: 0,
            },
            parent_index_number: None,
            index_number: None,
            series_id: None,
            series_name: None,
        }
    }

    fn library() -> Vec<Item> {
        vec![
            item("Spy x Family", "Series", 2022, false),
            item("Frieren: Beyond Journey's End", "Series", 2023, false),
            item("Dune", "Movie", 2021, true),
            item("Dune: Part Two", "Movie", 2024, false),
            item("Office Space", "Movie", 1999, false),
            item("The Lord of the Rings: The Two Towers", "Movie", 2002, false),
            item("The Lord of the Rings: The Fellowship of the Ring", "Movie", 2001, true),
            item("The Lord of the Rings: The Return of the King", "Movie", 2003, false),
        ]
    }

    #[test]
    fn spoken_titles_match() {
        let lib = library();
        let name = |q: &str| best_match(q, &lib).map(|i| i.name.as_str());
        assert_eq!(name("spy x family"), Some("Spy x Family"));
        assert_eq!(name("spy family"), Some("Spy x Family"));
        assert_eq!(name("spy by family"), Some("Spy x Family"));
        assert_eq!(name("spyxfamily"), Some("Spy x Family"));
        assert_eq!(name("frieren"), Some("Frieren: Beyond Journey's End"));
        assert_eq!(name("dune"), Some("Dune"));
        assert_eq!(name("dune part two"), Some("Dune: Part Two"));
        assert_eq!(name("dune part 2"), Some("Dune: Part Two"));
        // Fellowship is watched, so the oldest unwatched one.
        assert_eq!(name("lord of the rings"), Some("The Lord of the Rings: The Two Towers"));
        assert_eq!(name("breaking bad"), None);
        assert_eq!(name("the office"), None, "Office Space shares only one of two words");
    }

    #[test]
    fn episodes_without_a_name_parse() {
        // Seen on a real server: one episode of "One Piece" had no Name, and
        // the whole episode list failed to parse.
        let list: Items =
            serde_json::from_str(r#"{"Items": [{"Id": "e1", "Type": "Episode", "IndexNumber": 3}]}"#).unwrap();
        assert_eq!(list.items[0].name, "");
        assert_eq!(list.items[0].index_number, Some(3));
    }

    #[test]
    fn sound_and_semantic_title_checks() {
        let mut lib = library();
        lib.push(item("Futurama", "Series", 1999, false));
        assert_eq!(
            sound_match("freeran", &lib).map(|i| i.name.as_str()),
            Some("Frieren: Beyond Journey's End")
        );
        assert_eq!(
            sound_match("fryren", &lib).map(|i| i.name.as_str()),
            Some("Frieren: Beyond Journey's End")
        );
        assert!(sound_match("xy", &lib).is_none(), "too short");
        assert!(semantic_title_ok(
            "harry potter",
            "Harry Potter and the Chamber of Secrets"
        ));
        assert!(semantic_title_ok("arcain", "Arcane"));
        assert!(!semantic_title_ok("fryren", "Futurama"));
        assert!(!semantic_title_ok("game of thrones", "House of the Dragon"));
        assert!(!semantic_title_ok("breaking bad", "Franklin & Bash"));
        assert!(!semantic_title_ok("the office", "La oficina"));
        assert!(semantic_title_ok("anime about a spy who adopts a girl", "Spy x Family"));
    }

    #[test]
    fn fuzzy_titles() {
        let mut lib = library();
        lib.push(item("Interstellar", "Movie", 2014, false));
        let name = |q: &str| fuzzy_match(q, &lib).map(|(i, _)| i.name.clone());
        // All three Lord of the Rings films share the main title: the oldest unwatched one wins.
        assert_eq!(
            name("lord of the rinks").as_deref(),
            Some("The Lord of the Rings: The Two Towers")
        );
        assert_eq!(name("intersteller").as_deref(), Some("Interstellar"));
        assert_eq!(
            name("frieren beyond journeys end").as_deref(),
            Some("Frieren: Beyond Journey's End")
        );
        assert_eq!(name("the office"), None);
        assert_eq!(name("zebra"), None);
        assert!(fuzzy_match("dune", &lib).is_some_and(|(_, s)| (s - 1.0).abs() < 1e-6));
    }

    #[test]
    fn user_of_the_open_session() {
        let sessions = serde_json::json!([
            {"DeviceName": "Firefox", "UserName": "Schaka", "NowPlayingItem": {"Name": "x"}},
            {"DeviceName": "UGOOS-AM6", "UserName": "Schaka"},
            {"DeviceName": "UGOOS-AM6", "UserName": "Holly", "NowPlayingItem": {"Name": "y"}},
            {"DeviceName": "Nokia Streaming Box 8010", "UserName": "Holly"},
        ]);
        let s = sessions.as_array().unwrap();
        assert_eq!(
            session_user_of(s, "ugoos-am6, Kodi (CoreELEC)").as_deref(),
            Some("Holly")
        );
        assert_eq!(session_user_of(s, "Nokia Streaming Box 8010").as_deref(), Some("Holly"));
        assert_eq!(session_user_of(s, "SHIELD"), None);
    }

    #[test]
    fn season_and_episode() {
        assert_eq!(
            split_episode("Spy x Family season 2 episode 3"),
            ("Spy x Family".into(), Some(2), Some(3))
        );
        assert_eq!(split_episode("frieren episode five"), ("frieren".into(), None, Some(5)));
        assert_eq!(split_episode("Dune: Part Two"), ("Dune: Part Two".into(), None, None));
    }

    /// Against a real server: `JELLYFIN_URL=... JELLYFIN_API_KEY=... JELLYFIN_USER=...
    /// JELLYFIN_TEST_QUERY="spy family" cargo test live_server -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "needs a Jellyfin server"]
    async fn live_server() {
        let env = |k: &str| std::env::var(k).unwrap_or_default();
        let jf = Jellyfin::new(JellyfinConfig {
            url: env("JELLYFIN_URL"),
            api_key: env("JELLYFIN_API_KEY"),
            user: env("JELLYFIN_USER"),
        })
        .unwrap();
        for q in env("JELLYFIN_TEST_QUERY").split(';') {
            let devices = std::env::var("JELLYFIN_TEST_DEVICES").ok();
            println!("{q:?} -> {:?}", jf.search(q, None, devices.as_deref()).await.unwrap());
        }
    }
}
