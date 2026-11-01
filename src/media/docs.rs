//! The text that is embedded for each Jellyfin item, and Jellyfin's change
//! messages.

use serde_json::Value;

use super::embed;
use super::index::{Entry, Kind, fnv1a};

/// Changes when the layout of the text changes, so saved indexes are rebuilt.
pub const DOC_VERSION: u32 = 1;
/// Plot summaries are cut here. Longer text adds little for search.
const OVERVIEW_CHARS: usize = 800;
const ACTORS: usize = 5;

/// Fields that the documents need, for Jellyfin's `Fields` parameter.
pub const WORK_FIELDS: &str = "Overview,Genres,People,ProductionYear,OriginalTitle";
pub const EPISODE_FIELDS: &str = "Overview";

/// An item ready for embedding.
#[derive(Debug, Clone, PartialEq)]
pub struct Doc {
    pub entry: Entry,
    pub text: String,
}

fn cut(text: &str, max: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= max {
        return text;
    }
    let cut: String = text.chars().take(max).collect();
    match cut.rsplit_once(' ') {
        Some((head, _)) => format!("{head}…"),
        None => cut,
    }
}

fn strings(v: &Value) -> Vec<&str> {
    v.as_array().into_iter().flatten().filter_map(Value::as_str).collect()
}

/// The document of a Jellyfin series, movie or episode. None for other types
/// and for items without a name.
pub fn document(item: &Value) -> Option<Doc> {
    let kind = Kind::from_jellyfin(item["Type"].as_str()?)?;
    let id = item["Id"].as_str()?.to_owned();
    let name = item["Name"]
        .as_str()
        .filter(|n| !n.trim().is_empty())?
        .trim()
        .to_owned();
    let overview = cut(item["Overview"].as_str().unwrap_or(""), OVERVIEW_CHARS);
    let (title, mut parts, series_id, series_name) = match kind {
        Kind::Series | Kind::Movie => {
            let mut parts = Vec::new();
            let what = if kind == Kind::Series { "Series" } else { "Movie" };
            parts.push(match item["ProductionYear"].as_i64() {
                Some(y) => format!("{what}, {y}"),
                None => what.to_owned(),
            });
            if let Some(orig) = item["OriginalTitle"].as_str()
                && !orig.trim().is_empty()
                && orig.trim() != name
            {
                parts.push(format!("Original title: {}", orig.trim()));
            }
            let genres = strings(&item["Genres"]);
            if !genres.is_empty() {
                parts.push(format!("Genres: {}", genres.join(", ")));
            }
            let actors: Vec<&str> = item["People"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|p| p["Type"] == "Actor")
                .filter_map(|p| p["Name"].as_str())
                .take(ACTORS)
                .collect();
            if !actors.is_empty() {
                parts.push(format!("Starring {}", actors.join(", ")));
            }
            (name.clone(), parts, None, None)
        }
        Kind::Episode => {
            let series = item["SeriesName"].as_str().unwrap_or("").trim().to_owned();
            let mut what = String::from("Episode");
            if let Some(e) = item["IndexNumber"].as_i64() {
                what.push_str(&format!(" {e}"));
            }
            if let Some(s) = item["ParentIndexNumber"].as_i64() {
                what.push_str(&format!(" of season {s}"));
            }
            if !series.is_empty() {
                what.push_str(&format!(" of {series}"));
            }
            let title = if series.is_empty() {
                name.clone()
            } else {
                format!("{series}: {name}")
            };
            (
                title,
                vec![what],
                item["SeriesId"].as_str().map(str::to_owned),
                Some(series).filter(|s| !s.is_empty()),
            )
        }
    };
    if !overview.is_empty() {
        parts.push(overview);
    }
    let text = embed::document_text(&title, &format!("{}.", parts.join(". ")).replace("..", "."));
    Some(Doc {
        entry: Entry {
            id,
            kind,
            name,
            series_id,
            series_name,
            hash: fnv1a(&text),
        },
        text,
    })
}

/// Item IDs from a `LibraryChanged` message of Jellyfin's WebSocket.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub changed: Vec<String>,
    pub removed: Vec<String>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.removed.is_empty()
    }
}

/// Parses one WebSocket message. None for other message types.
pub fn library_changes(message: &Value) -> Option<Changes> {
    if message["MessageType"] != "LibraryChanged" {
        return None;
    }
    let data = &message["Data"];
    let ids = |key: &str| strings(&data[key]).into_iter().map(str::to_owned).collect::<Vec<_>>();
    let mut changed = ids("ItemsAdded");
    changed.extend(ids("ItemsUpdated"));
    changed.sort();
    changed.dedup();
    Some(Changes {
        changed,
        removed: ids("ItemsRemoved"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn movie_document() {
        let item = json!({
            "Type": "Movie", "Id": "m1", "Name": "Dune", "ProductionYear": 2021,
            "OriginalTitle": "Dune", "Genres": ["Science Fiction", "Adventure"],
            "People": [
                {"Name": "Timothée Chalamet", "Type": "Actor"},
                {"Name": "Denis Villeneuve", "Type": "Director"},
                {"Name": "Zendaya", "Type": "Actor"}
            ],
            "Overview": "Paul Atreides travels to   the desert planet Arrakis."
        });
        let d = document(&item).unwrap();
        assert_eq!(d.entry.kind, Kind::Movie);
        assert_eq!(
            d.text,
            "title: Dune | text: Movie, 2021. Genres: Science Fiction, Adventure. Starring Timothée Chalamet, Zendaya. Paul Atreides travels to the desert planet Arrakis."
        );
        assert_eq!(d.entry.hash, fnv1a(&d.text));
    }

    #[test]
    fn series_with_original_title_and_no_overview() {
        let item = json!({"Type": "Series", "Id": "s1", "Name": "Spy x Family", "OriginalTitle": "SPY×FAMILY"});
        let d = document(&item).unwrap();
        assert_eq!(
            d.text,
            "title: Spy x Family | text: Series. Original title: SPY×FAMILY."
        );
    }

    #[test]
    fn episode_document() {
        let item = json!({
            "Type": "Episode", "Id": "e1", "Name": "Will They Pass or Fail?", "SeriesName": "Spy x Family",
            "SeriesId": "s1", "IndexNumber": 5, "ParentIndexNumber": 1, "Overview": "The interview day."
        });
        let d = document(&item).unwrap();
        assert_eq!(d.entry.series_id.as_deref(), Some("s1"));
        assert_eq!(
            d.text,
            "title: Spy x Family: Will They Pass or Fail? | text: Episode 5 of season 1 of Spy x Family. The interview day."
        );
    }

    #[test]
    fn skipped_items() {
        assert!(document(&json!({"Type": "Audio", "Id": "a", "Name": "Song"})).is_none());
        assert!(document(&json!({"Type": "Movie", "Id": "a", "Name": "  "})).is_none());
        assert!(document(&json!({"Type": "Movie", "Name": "No id"})).is_none());
    }

    #[test]
    fn long_overviews_are_cut_at_a_word() {
        let long = "word ".repeat(400);
        let d = document(&json!({"Type": "Movie", "Id": "m", "Name": "X", "Overview": long})).unwrap();
        assert!(d.text.len() < 900);
        assert!(
            d.text.ends_with("….") || d.text.ends_with('…'),
            "{}",
            &d.text[d.text.len() - 10..]
        );
    }

    #[test]
    fn websocket_messages() {
        let msg = json!({"MessageType": "LibraryChanged", "Data": {
            "ItemsAdded": ["a", "b"], "ItemsUpdated": ["b", "c"], "ItemsRemoved": ["d"],
            "FoldersAddedTo": [], "CollectionFolders": []
        }});
        assert_eq!(
            library_changes(&msg).unwrap(),
            Changes {
                changed: vec!["a".into(), "b".into(), "c".into()],
                removed: vec!["d".into()]
            }
        );
        assert!(library_changes(&json!({"MessageType": "ForceKeepAlive", "Data": 60})).is_none());
        assert!(
            library_changes(&json!({"MessageType": "LibraryChanged", "Data": {}}))
                .unwrap()
                .is_empty()
        );
    }
}
