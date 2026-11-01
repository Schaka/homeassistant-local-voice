//! Web search with SearXNG, embedded as a Python library through PyO3.
//!
//! SearXNG's own web server never starts. The hub loads the `searx` package
//! once, and every search is a function call in this process. SearXNG queries
//! its engines (DuckDuckGo, Brave, Wikipedia and others) in parallel threads.

use std::ffi::CString;
use std::path::Path;
use std::sync::LazyLock;

use anyhow::{Context, Result};
use chrono::Datelike;
use futures_util::future::BoxFuture;
use pyo3::prelude::*;
use pyo3::types::PyModule;
use regex::Regex;
use serde::Deserialize;

use crate::config::SearchConfig;

const SNIPPET_CHARS: usize = 300;

#[derive(Debug, Default, Deserialize)]
pub struct Results {
    #[serde(default)]
    pub answers: Vec<String>,
    #[serde(default)]
    pub infobox: Option<Infobox>,
    #[serde(default)]
    pub results: Vec<Hit>,
}

#[derive(Debug, Deserialize)]
pub struct Infobox {
    pub name: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct Hit {
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub published: String,
}

pub trait SearchBackend: Send + Sync {
    fn search(&self, query: String, max_results: usize) -> BoxFuture<'_, Result<Results>>;
}

/// The Python side: initializes SearXNG once and runs one search per call.
/// SearXNG's search code needs a Flask request context, but no running web
/// server, so a test request context of an empty Flask app is enough.
const BOOTSTRAP: &str = r#"
import json
import flask
import searx
import searx.engines
import searx.search
from searx.search.models import EngineRef, SearchQuery

searx.search.initialize()
_app = flask.Flask("assistant-hub")
_refs = [EngineRef(e.name, "general") for e in searx.engines.categories.get("general", []) if not e.disabled]


def _get(obj, key):
    return obj.get(key) if isinstance(obj, dict) else getattr(obj, key, None)


def search(query, lang, max_results):
    with _app.test_request_context("/search"):
        container = searx.search.Search(SearchQuery(query, _refs, lang=lang or "all", safesearch=1)).search()
    results = []
    for r in container.get_ordered_results()[:max_results]:
        published = _get(r, "publishedDate")
        results.append({
            "title": str(_get(r, "title") or ""),
            "url": str(_get(r, "url") or ""),
            "content": str(_get(r, "content") or ""),
            "published": published.isoformat() if hasattr(published, "isoformat") else str(published or ""),
        })
    answers = [str(_get(a, "answer")) for a in list(container.answers) if _get(a, "answer")]
    infobox = None
    for box in container.infoboxes[:1]:
        if box.get("content"):
            infobox = {"name": str(box.get("infobox") or "Info"), "content": str(box["content"])}
    return json.dumps({"answers": answers, "infobox": infobox, "results": results})
"#;

pub struct SearXng {
    search_fn: Py<PyAny>,
    language: String,
}

fn write_settings(path: &Path) -> Result<()> {
    let secret = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    // The limiter and the image proxy are off: only this process calls SearXNG.
    let settings = format!(
        "use_default_settings: true\n\
         general:\n  instance_name: \"voice search\"\n\
         server:\n  secret_key: \"{secret}\"\n  limiter: false\n  image_proxy: false\n\
         search:\n  safe_search: 1\n  formats: [html, json]\n"
    );
    std::fs::write(path, settings)?;
    Ok(())
}

/// Writes the SearXNG settings file and returns the environment variable that
/// points to it. main sets it before any thread starts.
pub fn prepare(cfg: &SearchConfig) -> Result<(&'static str, String)> {
    write_settings(&cfg.settings_path)?;
    Ok(("SEARXNG_SETTINGS_PATH", cfg.settings_path.display().to_string()))
}

impl SearXng {
    /// Loads SearXNG. Blocking (about 0.3 s), call it from a blocking thread.
    pub fn load(cfg: &SearchConfig) -> Result<Self> {
        let search_fn = Python::attach(|py| -> PyResult<Py<PyAny>> {
            let sys = py.import("sys")?;
            let path = sys.getattr("path")?;
            for (i, p) in cfg.python_paths.iter().enumerate() {
                path.call_method1("insert", (i, p.display().to_string()))?;
            }
            let code = CString::new(BOOTSTRAP).expect("bootstrap has no NUL");
            let module = PyModule::from_code(py, &code, c"assistant_hub_search.py", c"assistant_hub_search")?;
            Ok(module.getattr("search")?.unbind())
        })
        .map_err(|e| anyhow::anyhow!("cannot load SearXNG: {e}"))?;
        tracing::info!("web search: SearXNG loaded");
        Ok(Self {
            search_fn,
            language: cfg.language.clone(),
        })
    }

    fn search_blocking(&self, query: &str, max_results: usize) -> Result<Results> {
        let json: String = Python::attach(|py| {
            self.search_fn
                .call1(py, (query, self.language.as_str(), max_results))?
                .extract(py)
        })
        .map_err(|e: PyErr| anyhow::anyhow!("{e}"))?;
        serde_json::from_str(&json).context("bad result from SearXNG")
    }
}

/// Runs searches on blocking threads.
pub struct SearXngBackend(pub std::sync::Arc<SearXng>);

impl SearchBackend for SearXngBackend {
    fn search(&self, query: String, max_results: usize) -> BoxFuture<'_, Result<Results>> {
        let inner = self.0.clone();
        Box::pin(async move { tokio::task::spawn_blocking(move || inner.search_blocking(&query, max_results)).await? })
    }
}

/// Words that ask for the most recent state of something. Small models rarely
/// put the year into such a query, and without it the engines rank general
/// pages first ("who won the last football world cup" found only overview
/// pages; with "2026" appended, all top results named the 2026 winner).
static RECENCY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(last|latest|newest|recent|recently|current|currently|now|today|tonight|yesterday|this (year|week|month|season)|who (is|are) the|still)\b",
    )
    .unwrap()
});
static YEAR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(19|20)\d{2}\b").unwrap());

/// Makes a query about recent events name the current year. No year: append
/// it. An older year: replace it, because small models put the year of their
/// training data into such a query.
pub fn with_year(query: &str, year: i32) -> String {
    if !RECENCY.is_match(query) {
        return query.to_owned();
    }
    let years: Vec<i32> = YEAR.find_iter(query).filter_map(|m| m.as_str().parse().ok()).collect();
    match years.iter().max() {
        None => format!("{query} {year}"),
        Some(&max) if max < year => YEAR.replace_all(query, year.to_string().as_str()).into_owned(),
        Some(_) => query.to_owned(),
    }
}

pub fn current_year() -> i32 {
    chrono::Local::now().year()
}

fn trim(text: &str, limit: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= limit {
        return text;
    }
    let cut: String = text.chars().take(limit - 1).collect();
    let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(a, _)| a);
    format!("{cut}…")
}

/// The text the model sees: a date line, direct answers, one infobox and a few
/// trimmed results with their source and date.
pub fn format_results(results: &Results, max_results: usize, today: &str) -> String {
    let mut lines = Vec::new();
    for answer in &results.answers {
        lines.push(format!("Direct answer: {}", trim(answer, SNIPPET_CHARS)));
    }
    if let Some(box_) = &results.infobox {
        lines.push(format!("{}: {}", box_.name, trim(&box_.content, SNIPPET_CHARS)));
    }
    for (i, hit) in results.results.iter().take(max_results).enumerate() {
        let host = url::Url::parse(&hit.url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_owned()))
            .unwrap_or_default();
        let date: String = hit.published.chars().take(10).collect();
        let meta = [host.as_str(), date.as_str()]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!(
            "{}. {} ({meta}): {}",
            i + 1,
            trim(&hit.title, 120),
            trim(&hit.content, SNIPPET_CHARS)
        ));
    }
    if lines.is_empty() {
        return "No results.".to_owned();
    }
    // Small models trust their training data over search results. The date
    // line makes clear that the results are the newer source.
    let head = format!(
        "Search results from today, {today}. They are newer than your own knowledge. \
         If they disagree with it, answer from the results."
    );
    std::iter::once(head).chain(lines).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn year_rules() {
        assert_eq!(
            with_year("who won the last world cup", 2026),
            "who won the last world cup 2026"
        );
        assert_eq!(
            with_year("who won the last world cup 2022", 2026),
            "who won the last world cup 2026"
        );
        assert_eq!(with_year("latest iphone 2026", 2026), "latest iphone 2026");
        assert_eq!(with_year("capital of australia", 2026), "capital of australia");
    }

    #[test]
    fn formatting() {
        let r = Results {
            answers: vec!["Canberra".into()],
            infobox: None,
            results: vec![Hit {
                title: "Canberra - Wikipedia".into(),
                url: "https://en.wikipedia.org/wiki/Canberra".into(),
                content: "Canberra is the capital city of Australia.".into(),
                published: "2026-10-01T00:00:00".into(),
            }],
        };
        let text = format_results(&r, 3, "2026-11-01");
        assert_eq!(
            text,
            "Search results from today, 2026-11-01. They are newer than your own knowledge. If they disagree with it, answer from the results.\n\
             Direct answer: Canberra\n\
             1. Canberra - Wikipedia (en.wikipedia.org, 2026-10-01): Canberra is the capital city of Australia."
        );
        assert_eq!(format_results(&Results::default(), 3, "x"), "No results.");
    }

    #[test]
    fn trimming() {
        assert_eq!(trim("a  b\nc", 10), "a b c");
        assert_eq!(trim("one two three four", 12), "one two…");
    }
}
