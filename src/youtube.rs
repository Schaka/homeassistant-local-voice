//! YouTube search for "play ... on YouTube".
//!
//! It uses InnerTube, the API behind the YouTube website. It needs no key. We
//! only need search results, not streams: the app on the player plays the
//! video. Rules for the pick:
//!
//! 1. The query is the name or the handle of a channel: the newest long video
//!    of that channel (its uploads playlist `UULF...` has no Shorts).
//! 2. A genre or mood ("some jazz", "lofi music"): the first livestream or
//!    video of at least 20 minutes.
//! 3. Anything else: the first video that is no Short and no upcoming premiere.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};

use crate::config::YoutubeConfig;

const LONG_MIX_SECS: u32 = 20 * 60;
const SHORT_SECS: u32 = 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Video {
    pub id: String,
    pub title: String,
    pub channel: String,
    pub duration_secs: Option<u32>,
    pub live: bool,
    pub upcoming: bool,
    pub short: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel {
    pub id: String,
    pub title: String,
    pub handle: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Video(Video),
    Channel(Channel),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    ChannelLatest,
    Live,
    Mix,
    Video,
}

#[derive(Debug, Clone, Serialize)]
pub struct Pick {
    pub kind: Kind,
    pub query: String,
    pub video_id: String,
    pub title: String,
    pub channel: String,
    pub url: String,
}

fn text(v: &Value) -> Option<String> {
    if let Some(s) = v["simpleText"].as_str() {
        return Some(s.to_owned());
    }
    if let Some(s) = v["content"].as_str() {
        return Some(s.to_owned());
    }
    let runs = v["runs"].as_array()?;
    Some(runs.iter().filter_map(|r| r["text"].as_str()).collect())
}

/// "1:02:03" or "28:14" in seconds.
pub fn parse_duration(s: &str) -> Option<u32> {
    let mut total = 0u32;
    for part in s.trim().split(':') {
        total = total.checked_mul(60)?.checked_add(part.parse().ok()?)?;
    }
    Some(total)
}

/// Every value under `key` anywhere inside `v`.
fn find_all<'a>(v: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                if k == key {
                    out.push(child);
                }
                find_all(child, key, out);
            }
        }
        Value::Array(list) => list.iter().for_each(|c| find_all(c, key, out)),
        _ => {}
    }
}

fn video_renderer(v: &Value) -> Option<Video> {
    let id = v["videoId"].as_str()?.to_owned();
    let duration_secs = text(&v["lengthText"]).and_then(|s| parse_duration(&s));
    let live = v["badges"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|b| b["metadataBadgeRenderer"]["style"] == "BADGE_STYLE_TYPE_LIVE_NOW")
        || v["thumbnailOverlays"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|o| o["thumbnailOverlayTimeStatusRenderer"]["style"] == "LIVE");
    let url = v["navigationEndpoint"]["commandMetadata"]["webCommandMetadata"]["url"]
        .as_str()
        .unwrap_or("");
    let short = !v["navigationEndpoint"]["reelWatchEndpoint"].is_null()
        || url.starts_with("/shorts/")
        || duration_secs.is_some_and(|d| d <= SHORT_SECS);
    Some(Video {
        id,
        title: text(&v["title"]).unwrap_or_default(),
        channel: text(&v["ownerText"])
            .or_else(|| text(&v["longBylineText"]))
            .unwrap_or_default(),
        duration_secs,
        live,
        upcoming: !v["upcomingEventData"].is_null(),
        short,
    })
}

/// The newer "lockup" layout, used for upload lists and some search results.
fn lockup(v: &Value) -> Option<Video> {
    if v["contentType"] != "LOCKUP_CONTENT_TYPE_VIDEO" {
        return None;
    }
    let id = v["contentId"].as_str()?.to_owned();
    let meta = &v["metadata"]["lockupMetadataViewModel"];
    let mut badges = Vec::new();
    find_all(&v["contentImage"], "thumbnailBadgeViewModel", &mut badges);
    let badge_texts: Vec<String> = badges
        .iter()
        .filter_map(|b| b["text"].as_str().map(str::to_uppercase))
        .collect();
    let mut parts = Vec::new();
    find_all(&meta["metadata"], "metadataParts", &mut parts);
    let part_texts: Vec<String> = parts
        .iter()
        .filter_map(|p| p.as_array())
        .flatten()
        .filter_map(|p| p["text"]["content"].as_str().map(str::to_owned))
        .collect();
    let duration_secs = badge_texts.iter().find_map(|t| parse_duration(t));
    let upcoming = badge_texts
        .iter()
        .any(|t| t.contains("UPCOMING") || t.contains("PREMIERE"))
        || part_texts
            .iter()
            .any(|t| t.starts_with("Premieres") || t.starts_with("Scheduled"));
    Some(Video {
        id,
        title: text(&meta["title"]).unwrap_or_default(),
        channel: part_texts.first().cloned().unwrap_or_default(),
        duration_secs,
        live: badge_texts.iter().any(|t| t == "LIVE"),
        upcoming,
        short: duration_secs.is_some_and(|d| d <= SHORT_SECS),
    })
}

fn channel_renderer(v: &Value) -> Option<Channel> {
    Some(Channel {
        id: v["channelId"].as_str()?.to_owned(),
        title: text(&v["title"]).unwrap_or_default(),
        handle: v["navigationEndpoint"]["browseEndpoint"]["canonicalBaseUrl"]
            .as_str()
            .unwrap_or("")
            .trim_start_matches('/')
            .to_owned(),
    })
}

fn section_items(sections: &Value) -> impl Iterator<Item = &Value> {
    sections
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s["itemSectionRenderer"]["contents"].as_array())
        .flatten()
}

/// Channels and videos of a search answer, in YouTube's order. Shelves (Shorts,
/// "people also watched") are skipped.
pub fn parse_search(answer: &Value) -> Vec<Item> {
    let sections =
        &answer["contents"]["twoColumnSearchResultsRenderer"]["primaryContents"]["sectionListRenderer"]["contents"];
    section_items(sections)
        .filter_map(|item| {
            if let Some(v) = item.get("videoRenderer") {
                video_renderer(v).map(Item::Video)
            } else if let Some(c) = item.get("channelRenderer") {
                channel_renderer(c).map(Item::Channel)
            } else if let Some(l) = item.get("lockupViewModel") {
                lockup(l).map(Item::Video)
            } else {
                None
            }
        })
        .collect()
}

/// Videos of an uploads playlist (browse `VLUULF...`), newest first.
pub fn parse_uploads(answer: &Value) -> Vec<Video> {
    let mut out = Vec::new();
    for tab in answer["contents"]["twoColumnBrowseResultsRenderer"]["tabs"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let sections = &tab["tabRenderer"]["content"]["sectionListRenderer"]["contents"];
        for item in section_items(sections) {
            if let Some(l) = item.get("lockupViewModel") {
                out.extend(lockup(l));
            } else if let Some(p) = item.get("playlistVideoRenderer") {
                out.extend(video_renderer(p));
            } else if let Some(list) = item["playlistVideoListRenderer"]["contents"].as_array() {
                out.extend(list.iter().filter_map(|i| video_renderer(&i["playlistVideoRenderer"])));
            }
        }
    }
    out
}

/// Spoken numbers as digits, so "level one techs" can match "Level1Techs".
const NUMBER_WORDS: &[(&str, &str)] = &[
    ("zero", "0"),
    ("one", "1"),
    ("two", "2"),
    ("three", "3"),
    ("four", "4"),
    ("five", "5"),
    ("six", "6"),
    ("seven", "7"),
    ("eight", "8"),
    ("nine", "9"),
    ("ten", "10"),
];

/// Lowercase letters and digits only, with spoken numbers as digits.
fn norm(s: &str) -> String {
    s.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| {
            NUMBER_WORDS
                .iter()
                .find(|(word, _)| *word == w)
                .map_or(w, |(_, digit)| digit)
        })
        .collect()
}

/// True if all characters of `short` occur in `long` in the same order.
fn is_subsequence(short: &str, long: &str) -> bool {
    let mut rest = long.chars();
    short.chars().all(|c| rest.any(|l| l == c))
}

/// Shortest abbreviation, as a share of the channel name, that still counts:
/// "lvl1techs" (9) for "level1techs" (11) counts, "jazz" for "jazzvibes" does not.
const ABBREVIATION_SHARE: f64 = 0.7;

fn name_matches(query: &str, name: &str) -> bool {
    if query.is_empty() || name.is_empty() {
        return false;
    }
    if query == name {
        return true;
    }
    // An abbreviation of the name: same first letter, letters in order, long enough.
    query.chars().next() == name.chars().next()
        && is_subsequence(query, name)
        && query.len() as f64 / name.len() as f64 >= ABBREVIATION_SHARE
}

/// The search words and whether the query asks for a genre or mood.
pub fn analyze(query: &str) -> (String, bool) {
    let q = query.trim().trim_end_matches(['.', '!', '?']).trim();
    let lower = q.to_lowercase();
    for prefix in ["some ", "any "] {
        if lower.starts_with(prefix) {
            return (q[prefix.len()..].trim().to_owned(), true);
        }
    }
    let genre = [" music", " mix", " radio", " playlist", " beats"]
        .iter()
        .any(|s| lower.ends_with(s));
    (q.to_owned(), genre)
}

/// The channel whose name or handle is the query, if the search found one.
/// Spoken numbers and abbreviations count ("level one techs", "lvl1techs").
pub fn matching_channel<'a>(query: &str, items: &'a [Item]) -> Option<&'a Channel> {
    let q = norm(query);
    let q_without_the = q.strip_prefix("the").unwrap_or(&q).to_owned();
    if q.is_empty() {
        return None;
    }
    items.iter().find_map(|item| match item {
        Item::Channel(c) => {
            let names = [norm(&c.title), norm(&c.handle)];
            let hit = names
                .iter()
                .any(|n| name_matches(&q, n) || name_matches(&q_without_the, n));
            hit.then_some(c)
        }
        Item::Video(_) => None,
    })
}

fn playable(v: &Video) -> bool {
    !v.short && !v.upcoming
}

/// Picks from search results. Rules 2 and 3 of the module comment.
pub fn pick_from_search(items: &[Item], genre: bool) -> Option<(Kind, &Video)> {
    let videos = || {
        items.iter().filter_map(|i| match i {
            Item::Video(v) if playable(v) => Some(v),
            _ => None,
        })
    };
    if genre && let Some(v) = videos().find(|v| v.live || v.duration_secs.is_some_and(|d| d >= LONG_MIX_SECS)) {
        return Some((if v.live { Kind::Live } else { Kind::Mix }, v));
    }
    videos()
        .next()
        .map(|v| (if v.live { Kind::Live } else { Kind::Video }, v))
}

pub struct YouTube {
    client: reqwest::Client,
    cfg: YoutubeConfig,
}

impl YouTube {
    pub fn new(cfg: YoutubeConfig) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .user_agent("Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0")
            .build()?;
        Ok(Self { client, cfg })
    }

    async fn call(&self, endpoint: &str, body: Value) -> Result<Value> {
        let mut payload = json!({
            "context": {"client": {
                "clientName": "WEB",
                "clientVersion": self.cfg.client_version,
                "hl": self.cfg.language,
                "gl": self.cfg.region,
            }},
        });
        if let (Value::Object(p), Value::Object(b)) = (&mut payload, body) {
            p.extend(b);
        }
        let resp = self
            .client
            .post(format!(
                "https://www.youtube.com/youtubei/v1/{endpoint}?prettyPrint=false"
            ))
            .header("X-YouTube-Client-Name", "1")
            .header("X-YouTube-Client-Version", &self.cfg.client_version)
            .json(&payload)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            bail!("YouTube {endpoint}: HTTP {status}");
        }
        resp.json()
            .await
            .with_context(|| format!("YouTube {endpoint}: bad JSON"))
    }

    pub async fn search(&self, query: &str) -> Result<Option<Pick>> {
        let (words, genre) = analyze(query);
        if words.is_empty() {
            return Ok(None);
        }
        let items = parse_search(&self.call("search", json!({"query": words})).await?);
        let make = |kind, v: &Video| Pick {
            kind,
            query: words.clone(),
            video_id: v.id.clone(),
            title: v.title.clone(),
            channel: v.channel.clone(),
            url: format!("https://www.youtube.com/watch?v={}", v.id),
        };
        if !genre
            && let Some(ch) = matching_channel(&words, &items)
            && let Some(rest) = ch.id.strip_prefix("UC")
        {
            match self.call("browse", json!({"browseId": format!("VLUULF{rest}")})).await {
                Ok(answer) => {
                    if let Some(v) = parse_uploads(&answer).iter().find(|v| playable(v) && !v.live) {
                        let mut pick = make(Kind::ChannelLatest, v);
                        pick.channel = ch.title.clone();
                        return Ok(Some(pick));
                    }
                }
                Err(err) => tracing::warn!("YouTube uploads of {}: {err:#}", ch.title),
            }
        }
        Ok(pick_from_search(&items, genre).map(|(kind, v)| make(kind, v)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("28:14"), Some(1694));
        assert_eq!(parse_duration("1:02:03"), Some(3723));
        assert_eq!(parse_duration("LIVE"), None);
    }

    #[test]
    fn query_analysis() {
        assert_eq!(analyze("some jazz"), ("jazz".into(), true));
        assert_eq!(analyze("lofi music"), ("lofi music".into(), true));
        assert_eq!(analyze("Linus Tech Tips."), ("Linus Tech Tips".into(), false));
    }

    #[test]
    fn channel_query_finds_the_channel_and_its_newest_upload() {
        let items = parse_search(&fixture("search_channel.json"));
        let ch = matching_channel("linus tech tips", &items).expect("channel");
        assert_eq!(ch.id, "UCXuqSBlHAE6Xw-yeJA0Tunw");
        assert_eq!(ch.handle, "@LinusTechTips");
        assert!(matching_channel("LinusTechTips", &items).is_some());
        let uploads = parse_uploads(&fixture("browse_uploads.json"));
        assert!(uploads.len() >= 2);
        let first = &uploads[0];
        assert_eq!(first.id, "H3Cnyj0I62c");
        assert!(first.duration_secs.is_some_and(|d| d > 60), "{first:?}");
        assert_eq!(first.channel, "Linus Tech Tips");
    }

    #[test]
    fn spoken_numbers_and_abbreviations_match_a_channel() {
        let items = vec![Item::Channel(Channel {
            id: "UC4w1YQAJMWOz4qtxinq55LQ".into(),
            title: "Level1Techs".into(),
            handle: "@Level1Techs".into(),
        })];
        for q in ["level one techs", "Level 1 Techs", "lvl1techs", "level1techs"] {
            assert!(matching_channel(q, &items).is_some(), "{q}");
        }
        for q in ["techs", "level", "linus tech tips"] {
            assert!(matching_channel(q, &items).is_none(), "{q}");
        }
        let jazz = vec![Item::Channel(Channel {
            id: "UCx".into(),
            title: "Jazz Vibes".into(),
            handle: "@jazzvibes".into(),
        })];
        assert!(matching_channel("jazz", &jazz).is_none());
    }

    #[test]
    fn plain_query_takes_the_first_video() {
        let items = parse_search(&fixture("search_plain.json"));
        assert!(matching_channel("how to fold a fitted sheet", &items).is_none());
        let (kind, v) = pick_from_search(&items, false).unwrap();
        assert_eq!(kind, Kind::Video);
        assert_eq!(v.id, "ckTCocBCUN4");
    }

    #[test]
    fn genre_query_takes_a_long_mix_or_a_livestream() {
        let items = parse_search(&fixture("search_genre.json"));
        let live: Vec<_> = items.iter().filter(|i| matches!(i, Item::Video(v) if v.live)).collect();
        assert!(!live.is_empty(), "the fixture has a 24/7 livestream");
        let (kind, v) = pick_from_search(&items, true).unwrap();
        assert!(matches!(kind, Kind::Mix | Kind::Live));
        assert!(v.live || v.duration_secs.unwrap() >= LONG_MIX_SECS);
    }
}
