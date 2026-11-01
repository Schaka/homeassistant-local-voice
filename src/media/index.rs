//! The vectors and what they belong to, in memory and on disk.
//!
//! On disk, the index is two files in `MEDIA_INDEX_DIR`:
//!
//! 1. `entries.json`: the fingerprint, the vector size and one record per
//!    item (ID, kind, names, the hash of its text).
//! 2. `vectors.f32`: the vectors of all records in the same order, as
//!    little-endian 32-bit floats.
//!
//! Both files are written to `*.tmp` first and then renamed. A file pair
//! with a different fingerprint (other model, other vector size, other text
//! layout) or a wrong length is ignored, and the index is built again.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::embed;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Kind {
    Series,
    Movie,
    Episode,
}

impl Kind {
    pub fn from_jellyfin(t: &str) -> Option<Self> {
        match t {
            "Series" => Some(Self::Series),
            "Movie" => Some(Self::Movie),
            "Episode" => Some(Self::Episode),
            _ => None,
        }
    }
}

/// One indexed item. `hash` is the FNV-1a hash of the text that was embedded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub kind: Kind,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series_name: Option<String>,
    pub hash: u64,
}

#[derive(Serialize, Deserialize)]
struct Header {
    fingerprint: String,
    dims: usize,
    entries: Vec<Entry>,
}

/// FNV-1a, 64 bit. Stable across Rust versions, unlike `DefaultHasher`.
pub fn fnv1a(text: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// A ranked match of a query.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub entry: Entry,
    pub score: f32,
}

#[derive(Debug, Default)]
pub struct Index {
    pub dims: usize,
    entries: Vec<Entry>,
    /// `entries.len() * dims` values.
    vectors: Vec<f32>,
    by_id: HashMap<String, usize>,
}

impl Index {
    pub fn new(dims: usize) -> Self {
        Self {
            dims,
            ..Default::default()
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.by_id.get(id).map(|&i| &self.entries[i])
    }

    pub fn count(&self, kind: Kind) -> usize {
        self.entries.iter().filter(|e| e.kind == kind).count()
    }

    /// Adds or replaces an entry.
    pub fn upsert(&mut self, entry: Entry, vector: &[f32]) -> Result<()> {
        if vector.len() != self.dims {
            bail!("vector has {} values, the index {}", vector.len(), self.dims);
        }
        match self.by_id.get(&entry.id) {
            Some(&i) => {
                self.entries[i] = entry;
                self.vectors[i * self.dims..(i + 1) * self.dims].copy_from_slice(vector);
            }
            None => {
                self.by_id.insert(entry.id.clone(), self.entries.len());
                self.entries.push(entry);
                self.vectors.extend_from_slice(vector);
            }
        }
        Ok(())
    }

    /// Removes entries. Returns how many were there.
    pub fn remove(&mut self, ids: &[String]) -> usize {
        let gone: std::collections::HashSet<&str> = ids.iter().map(String::as_str).collect();
        let before = self.entries.len();
        let mut entries = Vec::with_capacity(before);
        let mut vectors = Vec::with_capacity(self.vectors.len());
        for (i, e) in self.entries.drain(..).enumerate() {
            if !gone.contains(e.id.as_str()) {
                vectors.extend_from_slice(&self.vectors[i * self.dims..(i + 1) * self.dims]);
                entries.push(e);
            }
        }
        self.entries = entries;
        self.vectors = vectors;
        self.by_id = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
        before - self.entries.len()
    }

    /// The `limit` entries most similar to `query` (a normalized vector) that
    /// pass `keep`, best first.
    pub fn rank(&self, query: &[f32], limit: usize, keep: impl Fn(&Entry) -> bool) -> Vec<Hit> {
        if query.len() != self.dims || limit == 0 {
            return Vec::new();
        }
        let mut scored: Vec<(usize, f32)> = self
            .vectors
            .chunks_exact(self.dims)
            .map(|v| embed::dot(v, query))
            .enumerate()
            .filter(|(i, _)| keep(&self.entries[*i]))
            .collect();
        let by_score = |a: &(usize, f32), b: &(usize, f32)| b.1.total_cmp(&a.1);
        if scored.len() > limit {
            scored.select_nth_unstable_by(limit - 1, by_score);
            scored.truncate(limit);
        }
        scored.sort_by(by_score);
        scored
            .into_iter()
            .map(|(i, score)| Hit {
                entry: self.entries[i].clone(),
                score,
            })
            .collect()
    }

    pub fn save(&self, dir: &Path, fingerprint: &str) -> Result<()> {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let header = Header {
            fingerprint: fingerprint.to_owned(),
            dims: self.dims,
            entries: self.entries.clone(),
        };
        let (entries, vectors) = paths(dir);
        let tmp_e = entries.with_extension("json.tmp");
        let tmp_v = vectors.with_extension("f32.tmp");
        std::fs::write(&tmp_e, serde_json::to_vec(&header)?)?;
        let bytes: Vec<u8> = self.vectors.iter().flat_map(|f| f.to_le_bytes()).collect();
        std::fs::write(&tmp_v, bytes)?;
        // Vectors first: a crash between the renames leaves a length mismatch,
        // which `load` detects.
        std::fs::rename(&tmp_v, &vectors)?;
        std::fs::rename(&tmp_e, &entries)?;
        Ok(())
    }

    /// The saved index, or None if there is none or it does not fit.
    pub fn load(dir: &Path, fingerprint: &str, dims: usize) -> Option<Self> {
        let (entries, vectors) = paths(dir);
        let header: Header = serde_json::from_slice(&std::fs::read(&entries).ok()?).ok()?;
        if header.fingerprint != fingerprint || header.dims != dims {
            tracing::info!("media index: saved index is for another model or layout, building a new one");
            return None;
        }
        let raw = std::fs::read(&vectors).ok()?;
        if raw.len() != header.entries.len() * dims * 4 {
            tracing::warn!("media index: vector file has the wrong length, building a new one");
            return None;
        }
        let vectors = raw.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
        let by_id = header
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
        Some(Self {
            dims,
            entries: header.entries,
            vectors,
            by_id,
        })
    }
}

fn paths(dir: &Path) -> (PathBuf, PathBuf) {
    (dir.join("entries.json"), dir.join("vectors.f32"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, kind: Kind) -> Entry {
        Entry {
            id: id.into(),
            kind,
            name: id.into(),
            series_id: None,
            series_name: None,
            hash: fnv1a(id),
        }
    }

    #[test]
    fn hash_is_stable() {
        // Fixed value: a change would rebuild every saved index.
        assert_eq!(fnv1a("Spy x Family"), 0x94ef_b486_2674_a08f);
    }

    #[test]
    fn upsert_remove_rank() {
        let mut ix = Index::new(2);
        ix.upsert(entry("a", Kind::Movie), &[1.0, 0.0]).unwrap();
        ix.upsert(entry("b", Kind::Series), &[0.0, 1.0]).unwrap();
        ix.upsert(entry("c", Kind::Episode), &[0.6, 0.8]).unwrap();
        assert!(ix.upsert(entry("d", Kind::Movie), &[1.0]).is_err());
        let ranked = ix.rank(&[0.0, 1.0], 10, |_| true);
        assert_eq!(ix.rank(&[0.0, 1.0], 1, |_| true).len(), 1);
        assert_eq!(
            ranked.iter().map(|h| h.entry.id.as_str()).collect::<Vec<_>>(),
            ["b", "c", "a"]
        );
        // Replace keeps the position, remove keeps the rest aligned.
        ix.upsert(entry("a", Kind::Movie), &[0.0, 1.0]).unwrap();
        assert_eq!(ix.len(), 3);
        assert_eq!(ix.remove(&["b".into(), "zz".into()]), 1);
        let ranked = ix.rank(&[0.0, 1.0], 10, |_| true);
        assert_eq!(ranked[0].entry.id, "a");
        assert!((ranked[0].score - 1.0).abs() < 1e-6);
        assert_eq!(ix.get("c").unwrap().kind, Kind::Episode);
        let episodes = ix.rank(&[0.0, 1.0], 10, |e| e.kind == Kind::Episode);
        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].entry.id, "c");
        assert!(ix.get("b").is_none());
        assert_eq!(ix.count(Kind::Movie), 1);
    }

    #[test]
    fn save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let mut ix = Index::new(2);
        ix.upsert(entry("a", Kind::Movie), &[1.0, 0.0]).unwrap();
        ix.upsert(entry("b", Kind::Series), &[0.0, 1.0]).unwrap();
        ix.save(dir.path(), "fp1").unwrap();
        let back = Index::load(dir.path(), "fp1", 2).unwrap();
        assert_eq!(back.entries(), ix.entries());
        assert_eq!(back.rank(&[0.0, 1.0], 5, |_| true)[0].entry.id, "b");
        assert!(Index::load(dir.path(), "fp2", 2).is_none(), "other model");
        assert!(Index::load(dir.path(), "fp1", 3).is_none(), "other size");
        // A cut vector file is detected.
        let v = dir.path().join("vectors.f32");
        let raw = std::fs::read(&v).unwrap();
        std::fs::write(&v, &raw[..raw.len() - 4]).unwrap();
        assert!(Index::load(dir.path(), "fp1", 2).is_none());
        assert!(Index::load(&dir.path().join("missing"), "fp1", 2).is_none());
    }
}
