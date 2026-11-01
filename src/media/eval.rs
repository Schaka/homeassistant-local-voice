//! Evaluation of the whole Jellyfin search against a real library and the
//! real model. It is a test that only runs on request:
//!
//! ```text
//! JELLYFIN_URL=http://host:8096 JELLYFIN_API_KEY=... JELLYFIN_USER=alex \
//! EMBED_TEST_LIB=/opt/llama.cpp/libllama.so EMBED_TEST_MODEL=/models/embeddinggemma-2-Q8_0.gguf \
//! EVAL_INDEX_DIR=/tmp/media-index EVAL_THREADS=8 \
//! cargo test --release eval_real_library -- --ignored --nocapture
//! ```
//!
//! The labeled queries below fit the library of the author. For another
//! library, set `EVAL_CASES` to a file with one case per line:
//! `category|query|expected title prefix` (empty prefix: must find nothing,
//! `title#SxE` for an episode).

use std::path::PathBuf;
use std::sync::Arc;

use super::*;
use crate::config::JellyfinConfig;

const CASES: &str = "\
exact|spy x family|Spy x Family
exact|dune part two|Dune: Part Two
exact|attack on titan|Attack on Titan
exact|the bear|The Bear
exact|severance|Severance
exact|encanto|Encanto
exact|barbie|Barbie
spoken|spy family|Spy x Family
spoken|frieren|Frieren
spoken|demon slayer|Demon Slayer
spoken|your name|Your Name
spoken|princess mononoke|Princess Mononoke
spoken|harry potter|Harry Potter
spoken|moana two|Moana 2
spoken|lord of the rings|The Lord of the Rings
typo|spy famly|Spy x Family
typo|freeran|Frieren
typo|fryren|Frieren
typo|lord of the rinks|The Lord of the Rings
typo|intersteller|Interstellar
typo|squid gaem|Squid Game
typo|john wik|John Wick
typo|how to trane your dragon|How to Train Your Dragon
typo|the dragon prints|The Dragon Prince
typo|house of the dragons|House of the Dragon
typo|dune part to|Dune: Part Two
typo|atack on titen|Attack on Titan
typo|demon slayor|Demon Slayer
typo|severence|Severance
typo|arcain|Arcane
typo|plannet earth|Planet Earth
describe|anime about a spy who adopts a telepathic girl|Spy x Family
describe|that desert planet movie with the giant sand worms|Dune
describe|an elf mage travelling after the hero died|Frieren
describe|the one with Keanu Reeves as a hitman avenging his dog|John Wick
describe|korean thriller about a deadly game for money|Squid Game
describe|space movie about a farmer astronaut and a black hole|Interstellar
describe|anime about kids fighting giant titans behind walls|Attack on Titan
describe|a boy becomes a demon slayer after his family is killed|Demon Slayer
describe|a chef running a sandwich shop in chicago|The Bear
describe|office workers whose memories are split between work and home|Severance
describe|roman general who becomes a gladiator|Gladiator
describe|colombian family with magical powers|Encanto
describe|pirates searching for the one piece treasure|One Piece
episode|the spy x family episode where they go to penguin park|Spy x Family#1x12
episode|the spy family episode with the dodgeball game|Spy x Family#1x10
episode|the episode where anya meets a dog that can see the future|Spy x Family#1x14
none|breaking bad|
none|the office|
none|game of thrones|
none|the sopranos|
none|stranger things|
none|inception|
none|the matrix|
none|spirited away|
none|ted lasso|
none|cars|
";

struct Case {
    category: String,
    query: String,
    title: String,
    episode: Option<(u32, u32)>,
}

fn cases() -> Vec<Case> {
    let text = match std::env::var("EVAL_CASES") {
        Ok(path) => std::fs::read_to_string(path).expect("EVAL_CASES file"),
        Err(_) => CASES.to_owned(),
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut parts = l.splitn(3, '|');
            let category = parts.next().unwrap_or("").to_owned();
            let query = parts.next().unwrap_or("").to_owned();
            let expected = parts.next().unwrap_or("").to_owned();
            let (title, episode) = match expected.split_once('#') {
                Some((t, se)) => {
                    let (s, e) = se.split_once('x').expect("SxE");
                    (t.to_owned(), Some((s.parse().unwrap(), e.parse().unwrap())))
                }
                None => (expected, None),
            };
            Case {
                category,
                query,
                title,
                episode,
            }
        })
        .collect()
}

#[tokio::test]
#[ignore = "needs a Jellyfin server, libllama.so and the model"]
async fn eval_real_library() {
    let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("set {k}"));
    let jf = Arc::new(
        Jellyfin::new(JellyfinConfig {
            url: env("JELLYFIN_URL"),
            api_key: env("JELLYFIN_API_KEY"),
            user: env("JELLYFIN_USER"),
        })
        .unwrap(),
    );
    let threads: i32 = std::env::var("EVAL_THREADS")
        .ok()
        .and_then(|t| t.parse().ok())
        .unwrap_or(8);
    let mut gate = Gate::default();
    if let Some(s) = std::env::var("EVAL_MIN_SCORE").ok().and_then(|v| v.parse().ok()) {
        gate.min_score = s;
    }
    if let Some(m) = std::env::var("EVAL_MIN_MARGIN").ok().and_then(|v| v.parse().ok()) {
        gate.min_margin = m;
    }
    let cfg = MediaConfig {
        lib_llama: PathBuf::from(env("EMBED_TEST_LIB")),
        model: PathBuf::from(env("EMBED_TEST_MODEL")),
        model_url: String::new(),
        dims: std::env::var("EVAL_DIMS")
            .ok()
            .and_then(|d| d.parse().ok())
            .unwrap_or(512),
        threads,
        dir: PathBuf::from(std::env::var("EVAL_INDEX_DIR").unwrap_or_else(|_| "/tmp/media-index-eval".into())),
        episodes: std::env::var("EVAL_EPISODES").map(|v| v != "off").unwrap_or(true),
        gate,
    };
    let media = MediaIndex::new(cfg.clone(), jf.clone());
    media.load_model().await.unwrap();
    if let Some(saved) = Index::load(&cfg.dir, &media.fingerprint, cfg.dims) {
        *media.index.write().unwrap() = saved;
    }
    let started = std::time::Instant::now();
    media.full_sync().await.unwrap();
    println!("sync: {:.0} s, {}", started.elapsed().as_secs_f64(), media.status());
    jf.attach_media(media.clone());

    let mut by_cat: std::collections::BTreeMap<String, (usize, usize)> = Default::default();
    let mut failures = Vec::new();
    for c in cases() {
        let t = std::time::Instant::now();
        let pick = jf.search(&c.query, None, None).await.unwrap();
        let ms = t.elapsed().as_millis();
        let sem = media.semantic(&c.query).await.unwrap();
        let ok = match (&pick, c.title.is_empty()) {
            (None, true) => true,
            (Some(_), true) | (None, false) => false,
            (Some(p), false) => {
                p.title.to_lowercase().starts_with(&c.title.to_lowercase())
                    && c.episode
                        .is_none_or(|(s, e)| p.season == Some(s) && p.episode == Some(e))
            }
        };
        let entry = by_cat.entry(c.category.clone()).or_default();
        entry.1 += 1;
        if ok {
            entry.0 += 1;
        }
        let got = pick.as_ref().map_or("nothing".to_owned(), |p| {
            format!(
                "{} S{:?}E{:?} via {:?} {:.2}",
                p.title, p.season, p.episode, p.matched, p.score
            )
        });
        let sem_s = sem.as_ref().map_or("-".to_owned(), |s| {
            format!("{} score {:.3} margin {:.3}", s.name, s.score, s.margin)
        });
        println!(
            "{} {:9} {:60} -> {} [{ms} ms] | semantic: {sem_s}",
            if ok { "OK  " } else { "FAIL" },
            c.category,
            c.query,
            got
        );
        if !ok {
            let episodes = episode_intent(&c.query);
            let top = media
                .rank(&c.query, 5, |e| (e.kind == Kind::Episode) == episodes)
                .await
                .unwrap()
                .iter()
                .map(|h| {
                    format!(
                        "{} {:.3}",
                        h.entry
                            .series_name
                            .clone()
                            .map_or(h.entry.name.clone(), |s| format!("{s}: {}", h.entry.name)),
                        h.score
                    )
                })
                .collect::<Vec<_>>()
                .join(" | ");
            println!("       top 5: {top}");
            failures.push(format!("{}: {} -> {got}", c.category, c.query));
        }
    }
    println!("\nresults:");
    let (mut good, mut all) = (0, 0);
    for (cat, (g, n)) in &by_cat {
        println!("  {cat:9} {g}/{n}");
        good += g;
        all += n;
    }
    println!("  total     {good}/{all}");
    for f in &failures {
        println!("  failed: {f}");
    }
    let min: f64 = std::env::var("EVAL_MIN_ACCURACY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.9);
    assert!(good as f64 / all as f64 >= min, "accuracy below {min}");
}
