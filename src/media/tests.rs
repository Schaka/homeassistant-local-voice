//! Tests of the media index against a fake Jellyfin server and a fake embedder.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Query, State as AxState};
use axum::routing::get;
use serde_json::{Value, json};

use super::*;
use crate::config::JellyfinConfig;

const DIMS: usize = 64;

/// A bag of words, hashed into 64 values. Texts that share words get similar
/// vectors, which is enough to test ranking, gating and syncing.
struct FakeEmbedder {
    calls: AtomicUsize,
}

impl Embedder for FakeEmbedder {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(texts.len(), Ordering::SeqCst);
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = vec![0.0f32; DIMS];
                for w in crate::text::words(t) {
                    if matches!(w.as_str(), "task" | "search" | "result" | "query" | "title" | "text") {
                        continue;
                    }
                    v[(index::fnv1a(&w) % DIMS as u64) as usize] += 1.0;
                }
                v[0] += 0.05; // never all zero
                embed::normalize(&v)
            })
            .collect())
    }

    fn dims(&self) -> usize {
        DIMS
    }
}

#[derive(Clone, Default)]
struct FakeJellyfin {
    items: Arc<Mutex<Vec<Value>>>,
    requests: Arc<AtomicUsize>,
}

fn movie(id: &str, name: &str, overview: &str) -> Value {
    json!({"Type": "Movie", "Id": id, "Name": name, "Overview": overview})
}

fn series(id: &str, name: &str, overview: &str) -> Value {
    json!({"Type": "Series", "Id": id, "Name": name, "Overview": overview})
}

fn episode(id: &str, series_id: &str, series: &str, name: &str, overview: &str) -> Value {
    json!({"Type": "Episode", "Id": id, "Name": name, "SeriesId": series_id, "SeriesName": series,
           "IndexNumber": 1, "ParentIndexNumber": 1, "Overview": overview})
}

async fn items(AxState(f): AxState<FakeJellyfin>, Query(q): Query<HashMap<String, String>>) -> axum::Json<Value> {
    f.requests.fetch_add(1, Ordering::SeqCst);
    let types: Vec<String> = q
        .get("IncludeItemTypes")
        .map(|t| t.split(',').map(str::to_owned).collect())
        .unwrap_or_default();
    let ids: Option<Vec<String>> = q.get("Ids").map(|i| i.split(',').map(str::to_owned).collect());
    let all: Vec<Value> = f
        .items
        .lock()
        .unwrap()
        .iter()
        .filter(|i| types.iter().any(|t| i["Type"] == t.as_str()))
        .filter(|i| {
            ids.as_ref()
                .is_none_or(|ids| ids.iter().any(|id| i["Id"] == id.as_str()))
        })
        .cloned()
        .collect();
    let start: usize = q.get("StartIndex").and_then(|s| s.parse().ok()).unwrap_or(0);
    let limit: usize = q.get("Limit").and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);
    let page: Vec<Value> = all.iter().skip(start).take(limit).cloned().collect();
    axum::Json(json!({"Items": page, "TotalRecordCount": all.len()}))
}

async fn fake_server(fake: FakeJellyfin) -> String {
    let app = Router::new().route("/Items", get(items)).with_state(fake);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn jellyfin(url: &str) -> Arc<Jellyfin> {
    Arc::new(
        Jellyfin::new(JellyfinConfig {
            url: url.to_owned(),
            api_key: "key".into(),
            user: "alex".into(),
        })
        .unwrap(),
    )
}

fn config(dir: &std::path::Path) -> MediaConfig {
    MediaConfig {
        lib_llama: PathBuf::new(),
        model: PathBuf::from("fake.gguf"),
        model_url: String::new(),
        dims: DIMS,
        threads: 1,
        dir: dir.to_owned(),
        episodes: true,
        gate: Gate {
            min_score: 0.3,
            min_margin: 0.1,
        },
    }
}

fn library() -> Vec<Value> {
    vec![
        series(
            "s1",
            "Spy x Family",
            "A spy, an assassin and a telepath girl form a fake family.",
        ),
        episode(
            "e1",
            "s1",
            "Spy x Family",
            "Operation Strix",
            "Loid adopts Anya from an orphanage.",
        ),
        episode(
            "e2",
            "s1",
            "Spy x Family",
            "The Dog",
            "The family adopts a big white dog that sees the future.",
        ),
        movie(
            "m1",
            "Dune",
            "Paul travels to the desert planet Arrakis with giant sand worms.",
        ),
        movie(
            "m2",
            "Interstellar",
            "A farmer astronaut flies through a wormhole near a black hole.",
        ),
        movie(
            "m3",
            "Office Space",
            "Three office workers hate their jobs and plan revenge.",
        ),
    ]
}

async fn index_with(items: Vec<Value>, dir: &std::path::Path) -> (Arc<MediaIndex>, FakeJellyfin, Arc<FakeEmbedder>) {
    let fake = FakeJellyfin::default();
    *fake.items.lock().unwrap() = items;
    let url = fake_server(fake.clone()).await;
    let ix = MediaIndex::new(config(dir), jellyfin(&url));
    let emb = Arc::new(FakeEmbedder {
        calls: AtomicUsize::new(0),
    });
    ix.use_embedders(emb.clone(), emb.clone()).unwrap();
    (ix, fake, emb)
}

#[tokio::test]
async fn full_sync_embeds_only_new_and_changed_items() {
    let dir = tempfile::tempdir().unwrap();
    let (ix, fake, emb) = index_with(library(), dir.path()).await;
    assert!(!ix.ready());
    ix.full_sync().await.unwrap();
    assert!(ix.ready());
    assert_eq!(emb.calls.load(Ordering::SeqCst), 6);
    assert_eq!(ix.state(), State::Ready);

    // Nothing changed: no embedding.
    ix.full_sync().await.unwrap();
    assert_eq!(emb.calls.load(Ordering::SeqCst), 6);

    // One plot changes, one movie disappears, one appears.
    {
        let mut items = fake.items.lock().unwrap();
        items[3]["Overview"] = json!("A new plot about spice.");
        items.retain(|i| i["Id"] != "m3");
        items.push(movie("m4", "Coco", "A boy visits the land of the dead."));
    }
    ix.full_sync().await.unwrap();
    assert_eq!(emb.calls.load(Ordering::SeqCst), 8);
    let status = ix.status();
    assert_eq!(status["movies"], 3);
    assert_eq!(status["episodes"], 2);
    assert_eq!(status["status"]["state"], "ready");
}

#[tokio::test]
async fn paging_reads_the_whole_library() {
    let dir = tempfile::tempdir().unwrap();
    let many: Vec<Value> = (0..2500)
        .map(|i| movie(&format!("m{i}"), &format!("Movie {i}"), "plot"))
        .collect();
    let (ix, fake, emb) = index_with(many, dir.path()).await;
    ix.full_sync().await.unwrap();
    assert_eq!(emb.calls.load(Ordering::SeqCst), 2500);
    // 3 pages of movies and series, 1 empty page of episodes.
    assert_eq!(fake.requests.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn a_restart_loads_the_saved_index() {
    let dir = tempfile::tempdir().unwrap();
    let (ix, fake, _) = index_with(library(), dir.path()).await;
    ix.full_sync().await.unwrap();
    drop(ix);
    let url = fake_server(fake).await;
    let again = MediaIndex::new(config(dir.path()), jellyfin(&url));
    let emb = Arc::new(FakeEmbedder {
        calls: AtomicUsize::new(0),
    });
    again.use_embedders(emb.clone(), emb.clone()).unwrap();
    let saved = Index::load(dir.path(), &again.fingerprint, DIMS).unwrap();
    assert_eq!(saved.len(), 6);
    *again.index.write().unwrap() = saved;
    again.full_sync().await.unwrap();
    assert_eq!(emb.calls.load(Ordering::SeqCst), 0, "nothing to embed after a restart");
    // Another vector size or text layout means another fingerprint.
    let mut cfg = config(dir.path());
    cfg.episodes = false;
    let other = MediaIndex::new(cfg, jellyfin("http://127.0.0.1:1"));
    assert!(Index::load(dir.path(), &other.fingerprint, DIMS).is_none());
}

#[tokio::test]
async fn live_changes_are_applied() {
    let dir = tempfile::tempdir().unwrap();
    let (ix, fake, emb) = index_with(library(), dir.path()).await;
    ix.full_sync().await.unwrap();
    fake.items
        .lock()
        .unwrap()
        .push(movie("m9", "Coco", "A boy visits the land of the dead."));
    ix.apply(docs::Changes {
        changed: vec!["m9".into(), "m1".into()],
        removed: vec!["m3".into()],
    })
    .await
    .unwrap();
    // m9 is new, m1 has the same text as before.
    assert_eq!(emb.calls.load(Ordering::SeqCst), 7);
    let names: Vec<String> = ix
        .index
        .read()
        .unwrap()
        .entries()
        .iter()
        .map(|e| e.name.clone())
        .collect();
    assert!(names.contains(&"Coco".to_owned()));
    assert!(!names.contains(&"Office Space".to_owned()));
}

#[tokio::test]
async fn semantic_search_maps_episodes_to_their_series() {
    let dir = tempfile::tempdir().unwrap();
    let (ix, _, _) = index_with(library(), dir.path()).await;
    ix.full_sync().await.unwrap();

    let dog = ix.semantic("family adopts big white dog").await.unwrap().unwrap();
    assert_eq!((dog.id.as_str(), dog.kind), ("s1", Kind::Series), "{dog:?}");

    let ep = ix
        .semantic("the episode where the family adopts a big white dog")
        .await
        .unwrap()
        .unwrap();
    assert_eq!((ep.id.as_str(), ep.kind), ("e2", Kind::Episode), "{ep:?}");
    assert_eq!(ep.name, "Spy x Family: The Dog");

    let dune = ix.semantic("desert planet sand worms").await.unwrap().unwrap();
    assert_eq!(dune.id, "m1");
    assert!(dune.accepted, "{dune:?}");
}

#[test]
fn the_gate_needs_a_margin() {
    let mut ix = Index::new(2);
    let e = |id: &str| Entry {
        id: id.into(),
        kind: Kind::Movie,
        name: id.into(),
        series_id: None,
        series_name: None,
        hash: 0,
    };
    let hits = |scores: &[f32]| -> Vec<Hit> {
        scores
            .iter()
            .enumerate()
            .map(|(i, s)| Hit {
                entry: e(&format!("m{i}")),
                score: *s,
            })
            .collect()
    };
    for i in 0..30 {
        ix.upsert(e(&format!("m{i}")), &[1.0, 0.0]).unwrap();
    }
    let g = Gate {
        min_score: 0.6,
        min_margin: 0.05,
    };
    // One item stands out.
    let mut s = vec![0.80, 0.70, 0.69, 0.68, 0.67];
    s.extend(std::iter::repeat_n(0.66, 25));
    let r = gate(&hits(&s), &ix, false, g).unwrap();
    assert!(r.accepted && (r.margin - 0.14).abs() < 1e-4, "{r:?}");
    // Flat scores: a title that is not in the library.
    let flat: Vec<f32> = (0..30).map(|i| 0.72 - i as f32 * 0.001).collect();
    assert!(!gate(&hits(&flat), &ix, false, g).unwrap().accepted);
    // High margin but a low score.
    let mut low = vec![0.55];
    low.extend(std::iter::repeat_n(0.3, 29));
    assert!(!gate(&hits(&low), &ix, false, g).unwrap().accepted);
    assert!(gate(&[], &ix, false, g).is_none());
}

#[test]
fn episode_intent_words() {
    assert!(episode_intent("the episode where they adopt a dog"));
    assert!(episode_intent("the one where Loid meets Yor"));
    assert!(!episode_intent("spy family"));
    assert!(!episode_intent("anime about a spy who adopts a girl"));
}

#[tokio::test]
async fn websocket_changes_reach_the_index() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let dir = tempfile::tempdir().unwrap();
    let fake = FakeJellyfin::default();
    *fake.items.lock().unwrap() = library();
    // One listener that answers /Items over HTTP and /socket as a WebSocket.
    let app_fake = fake.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let auth_seen = Arc::new(Mutex::new(String::new()));
    let auth = auth_seen.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mut first = [0u8; 16];
            let n = stream.peek(&mut first).await.unwrap();
            let head = String::from_utf8_lossy(&first[..n]).to_string();
            if head.starts_with("GET /socket") {
                let auth = auth.clone();
                tokio::spawn(async move {
                    #[allow(clippy::result_large_err)] // tungstenite fixes the signature
                    let cb =
                        |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
                         resp: tokio_tungstenite::tungstenite::handshake::server::Response| {
                            *auth.lock().unwrap() = req.headers()["Authorization"].to_str().unwrap().to_owned();
                            Ok(resp)
                        };
                    let mut ws = tokio_tungstenite::accept_hdr_async(stream, cb).await.unwrap();
                    ws.send(Message::text(r#"{"MessageType":"ForceKeepAlive","Data":60}"#))
                        .await
                        .unwrap();
                    ws.send(Message::text(
                        r#"{"MessageType":"LibraryChanged","Data":{"ItemsAdded":["m9"],"ItemsUpdated":[],"ItemsRemoved":["m3"]}}"#,
                    ))
                    .await
                    .unwrap();
                    while ws.next().await.is_some() {}
                });
            } else {
                let app = Router::new().route("/Items", get(items)).with_state(app_fake.clone());
                tokio::spawn(async move {
                    let svc = hyper_util::service::TowerToHyperService::new(app);
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(stream), svc)
                        .await;
                });
            }
        }
    });
    fake.items
        .lock()
        .unwrap()
        .push(movie("m9", "Coco", "A boy visits the land of the dead."));
    let ix = MediaIndex::new(config(dir.path()), jellyfin(&format!("http://{addr}")));
    let emb = Arc::new(FakeEmbedder {
        calls: AtomicUsize::new(0),
    });
    ix.use_embedders(emb.clone(), emb.clone()).unwrap();
    ix.quiet_ms.store(50, Ordering::Relaxed);
    let cancel = CancellationToken::new();
    let task = tokio::spawn(ix.clone().run(cancel.clone()));
    let mut ok = false;
    // Office Space is still in Jellyfin's list, so only the WebSocket message removes it.
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let names: Vec<String> = ix
            .index
            .read()
            .unwrap()
            .entries()
            .iter()
            .map(|e| e.name.clone())
            .collect();
        if ix.state() == State::Ready
            && names.contains(&"Coco".to_owned())
            && !names.contains(&"Office Space".to_owned())
        {
            ok = true;
            break;
        }
    }
    cancel.cancel();
    let _ = task.await;
    assert!(ok, "the WebSocket change was not applied");
    assert!(auth_seen.lock().unwrap().contains("Token=\"key\""));
}

/// A fake Jellyfin with users, a library, next-up lists and Jellyfin's own search.
async fn full_fake_server(fake: FakeJellyfin, search_hit: Option<Value>) -> String {
    let lib = fake.clone();
    let lib2 = fake.clone();
    let one = fake.clone();
    let app = Router::new()
        .route(
            "/Users",
            get(|| async { axum::Json(json!([{"Name": "Alex", "Id": "u1"}])) }),
        )
        .route(
            "/System/Info/Public",
            get(|| async { axum::Json(json!({"Id": "srv1"})) }),
        )
        .route(
            "/Items",
            get(move |q: Query<HashMap<String, String>>| {
                let lib = lib.clone();
                let search_hit = search_hit.clone();
                async move {
                    if q.get("searchTerm").is_some() {
                        return axum::Json(
                            json!({"Items": search_hit.into_iter().collect::<Vec<_>>(), "TotalRecordCount": 1}),
                        );
                    }
                    items(AxState(lib), q).await
                }
            }),
        )
        .route(
            "/Items/{id}",
            get(move |axum::extract::Path(id): axum::extract::Path<String>| {
                let one = one.clone();
                async move {
                    let items = one.items.lock().unwrap();
                    axum::Json(
                        items
                            .iter()
                            .find(|i| i["Id"] == id.as_str())
                            .cloned()
                            .unwrap_or(json!({})),
                    )
                }
            }),
        )
        .route("/Shows/NextUp", get(|| async { axum::Json(json!({"Items": []})) }))
        .route(
            "/Shows/{id}/Episodes",
            get(move |axum::extract::Path(id): axum::extract::Path<String>| {
                let lib2 = lib2.clone();
                async move {
                    let items: Vec<Value> = lib2
                        .items
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|i| i["SeriesId"] == id.as_str())
                        .cloned()
                        .collect();
                    axum::Json(json!({"Items": items}))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn the_search_cascade_uses_each_stage() {
    let dir = tempfile::tempdir().unwrap();
    let fake = FakeJellyfin::default();
    let mut lib = library();
    lib.push(movie(
        "m5",
        "The Lord of the Rings: The Fellowship of the Ring",
        "Hobbits carry a ring to a volcano.",
    ));
    *fake.items.lock().unwrap() = lib;
    // Jellyfin's own search finds a movie that came after the title list was cached.
    let newer = movie("m6", "Coco", "A boy visits the land of the dead.");
    let url = full_fake_server(fake.clone(), Some(newer.clone())).await;
    let jf = jellyfin(&url);
    let ix = MediaIndex::new(config(dir.path()), jf.clone());
    let emb = Arc::new(FakeEmbedder {
        calls: AtomicUsize::new(0),
    });
    ix.use_embedders(emb.clone(), emb).unwrap();
    ix.full_sync().await.unwrap();
    jf.attach_media(ix.clone());
    let search = |q: &'static str| {
        let jf = jf.clone();
        async move { jf.search(q, None, None).await.unwrap() }
    };
    use crate::jellyfin::Match;

    let p = search("spy family").await.unwrap();
    assert_eq!((p.matched, p.title.as_str()), (Match::Title, "Spy x Family"));
    // No next-up list: the first episode.
    assert_eq!((p.season, p.episode), (Some(1), Some(1)));

    let p = search("lord of the rinks").await.unwrap();
    assert_eq!(p.matched, Match::Fuzzy, "{p:?}");
    assert!(p.title.starts_with("The Lord of the Rings"));

    let p = search("movie about a desert planet with giant sand worms")
        .await
        .unwrap();
    assert_eq!((p.matched, p.title.as_str()), (Match::Semantic, "Dune"), "{p:?}");

    let p = search("the episode where the family adopts a big white dog")
        .await
        .unwrap();
    assert_eq!(p.matched, Match::Semantic, "{p:?}");
    assert_eq!((p.title.as_str(), p.item_id.as_str()), ("Spy x Family", "e2"));

    // Newer than the cached title list: Jellyfin's search, accepted for a close title.
    fake.items.lock().unwrap().push(newer);
    let p = search("coco").await.unwrap();
    assert_eq!((p.matched, p.title.as_str()), (Match::Jellyfin, "Coco"), "{p:?}");

    // Jellyfin's search answers with Coco for anything, but "zebra" is not close to it.
    assert!(search("zebra crossing documentary").await.is_none());
}
