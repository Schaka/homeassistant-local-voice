//! The optional conversation agent: llama-server as a child process on an
//! internal port, reached through `/llm/...` on the main port.

pub mod engine;
pub mod eval;
#[cfg(test)]
pub mod fake;
pub mod ik;
pub mod llamacpp;
pub mod proxy;
pub mod slots;
pub mod specs;
pub mod template;

pub use llamacpp::server_args;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use crate::config::{Ctx, Family, LlmConfig, LlmSource};
use crate::supervisor::{ChildSpec, Health};

pub(crate) fn s(v: &str) -> String {
    v.to_owned()
}

/// Sampling values and chat template per family. They follow each model card.
/// Home Assistant sends its own temperature and top_p with each request, so
/// set the same values in its agent options (docs/llm.md).
pub(crate) fn preset(cfg: &LlmConfig) -> Vec<String> {
    let v: &[&str] = match (cfg.family, cfg.reasoning) {
        // llama.cpp's default min-p 0.05 causes repetition with MiniCPM5.
        (Family::Minicpm5, true) => &["--temp", "0.9", "--top-p", "0.95", "--min-p", "0"],
        (Family::Minicpm5, false) => &["--temp", "0.7", "--top-p", "0.95", "--min-p", "0"],
        // LFM3 card, "general (chat, tool use, agents)".
        (Family::Lfm3, _) => &["--temp", "0.1", "--top-k", "50", "--repeat-penalty", "1.1"],
        (Family::Qwen35, true) => &[
            "--temp",
            "1.0",
            "--top-p",
            "0.95",
            "--top-k",
            "20",
            "--min-p",
            "0",
            "--presence-penalty",
            "1.5",
        ],
        (Family::Qwen35, false) => &[
            "--temp",
            "0.7",
            "--top-p",
            "0.8",
            "--top-k",
            "20",
            "--min-p",
            "0",
            "--presence-penalty",
            "1.5",
        ],
        (Family::Ling3, _) => &["--temp", "1.0", "--top-p", "0.95", "--top-k", "20"],
        // Granite 4.0 H decodes greedily in IBM's examples.
        (Family::Granite4, _) => &["--temp", "0.0"],
    };
    v.iter().map(|x| s(x)).collect()
}

pub(crate) fn ctx_args(cfg: &LlmConfig) -> Vec<String> {
    let v: &[&str] = match (cfg.ctx, cfg.family) {
        (Ctx::K32, _) => &["-c", "32768"],
        (Ctx::K64, _) => &["-c", "65536"],
        (Ctx::K128, _) => &["-c", "131072"],
        // Ling 3.0 needs YaRN rope scaling (factor 2 over its 128k base).
        (Ctx::K256, Family::Ling3) => &[
            "-c",
            "262144",
            "--rope-scaling",
            "yarn",
            "--rope-scale",
            "2",
            "--yarn-orig-ctx",
            "131072",
        ],
        // Config::from_env allows 256k only for ling3 and lfm3.
        (Ctx::K256, _) => &["-c", "262144"],
    };
    v.iter().map(|x| s(x)).collect()
}

/// The `repo:tag` that `llama-server --cache-list` prints for this repo.
pub fn cache_key(repo: &str) -> String {
    let want = repo.to_lowercase();
    let last = want.rsplit('/').next().unwrap_or(&want);
    if last.contains(':') {
        want
    } else {
        format!("{want}:q4_k_m")
    }
}

/// Parses the output of `llama-server --cache-list` (lines like `  1. org/repo:q4_k_m`).
pub fn cache_list_contains(output: &str, key: &str) -> bool {
    output.lines().any(|line| {
        let line = line.trim_start();
        let Some((num, rest)) = line.split_once(". ") else {
            return false;
        };
        num.chars().all(|c| c.is_ascii_digit())
            && !num.is_empty()
            && rest.split_whitespace().next().map(str::to_lowercase).as_deref() == Some(key)
    })
}

async fn is_cached(cfg: &LlmConfig, repo: &str) -> bool {
    let out = tokio::process::Command::new(&cfg.server_bin)
        .arg("--cache-list")
        .env("LLAMA_CACHE", &cfg.hf_cache_dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await;
    match out {
        Ok(out) => cache_list_contains(&String::from_utf8_lossy(&out.stdout), &cache_key(repo)),
        Err(err) => {
            tracing::warn!("llama-server --cache-list failed: {err}");
            false
        }
    }
}

/// The GGUF file of a Hugging Face repo in llama.cpp's cache
/// (`models--org--repo/snapshots/<sha>/<file>.gguf`), if it was downloaded.
pub fn cached_gguf(cache_dir: &Path, repo: &str) -> Option<PathBuf> {
    let (name, tag) = match repo.split_once(':') {
        Some((n, t)) => (n, t.to_lowercase()),
        None => (repo, "q4_k_m".to_owned()),
    };
    let folder = cache_dir
        .join(format!("models--{}", name.replace('/', "--")))
        .join("snapshots");
    let mut found = Vec::new();
    for snapshot in std::fs::read_dir(folder).ok()?.flatten() {
        for file in std::fs::read_dir(snapshot.path()).ok()?.flatten() {
            let path = file.path();
            let fname = path.file_name()?.to_str()?.to_lowercase();
            if fname.ends_with(".gguf") && fname.contains(&tag) && !fname.contains("mmproj") {
                found.push(path);
            }
        }
    }
    found.sort();
    found.pop()
}

/// Reads the chat template from the model file, patches it for the family and
/// writes it next to the prompt cache. None: no patch needed or possible.
fn patched_template(cfg: &LlmConfig, model: &Path) -> Option<PathBuf> {
    let original = match crate::gguf::metadata_string(model, "tokenizer.chat_template") {
        Ok(Some(t)) => t,
        Ok(None) => {
            tracing::warn!(
                "{} has no chat template, llama-server uses its default",
                model.display()
            );
            return None;
        }
        Err(err) => {
            tracing::warn!("cannot read the chat template of {}: {err:#}", model.display());
            return None;
        }
    };
    match template::patch(cfg.family, &original) {
        template::Outcome::NotNeeded => None,
        template::Outcome::NoMatch(n) => {
            tracing::warn!(
                "the chat template of {} changed upstream ({n} matches instead of 1): it is used without the \
                 tool-result fix, answers after a tool call may be slow or empty",
                model.display()
            );
            None
        }
        template::Outcome::Patched(t) => {
            let path = std::env::temp_dir().join(format!("chat-template-{}.jinja", cfg.model_name));
            match std::fs::write(&path, t) {
                Ok(()) => {
                    tracing::info!(
                        "chat template of {} patched: no new thinking after a tool result",
                        model.display()
                    );
                    Some(path)
                }
                Err(err) => {
                    tracing::warn!("cannot write {}: {err}", path.display());
                    None
                }
            }
        }
    }
}

fn writable_dir(dir: &Path) -> bool {
    std::fs::create_dir_all(dir).is_ok() && tempfile_probe(dir)
}

fn tempfile_probe(dir: &Path) -> bool {
    let probe = dir.join(".write-test");
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// True if the prompt cache can be kept: it is on, and its folder is writable.
pub fn slots_usable(cfg: &LlmConfig) -> bool {
    cfg.slot_cache && {
        let ok = writable_dir(&cfg.slot_dir);
        if !ok {
            tracing::warn!(
                "{} is not writable, the prompt cache is not kept across restarts",
                cfg.slot_dir.display()
            );
        }
        ok
    }
}

/// The model file on disk: the configured file, or the download of a
/// Hugging Face repository in the cache of llama.cpp.
pub fn model_file(cfg: &LlmConfig) -> Option<PathBuf> {
    match &cfg.source {
        LlmSource::File(path) => path.is_file().then(|| path.clone()),
        LlmSource::HfRepo(repo) => cached_gguf(&cfg.hf_cache_dir, repo),
    }
}

/// The patched chat template of the model, if the fix is on and applies.
fn template_for(cfg: &LlmConfig, model_file: Option<&Path>) -> Option<PathBuf> {
    match (model_file, cfg.template_fix) {
        (Some(file), true) => patched_template(cfg, file),
        (None, true) => {
            tracing::info!("the model is not downloaded yet: the chat template fix applies from the next start");
            None
        }
        (_, false) => None,
    }
}

/// llama.cpp answers `/health` on its HTTP threads at once.
const LLAMACPP_HEALTH_TIMEOUT: Duration = Duration::from_secs(10);
/// ik_llama.cpp answers `/health` from the loop that also runs the model.
/// During a long prompt, the answer waits for the end of the current batch
/// of up to 2048 tokens. On an old CPU, that takes up to a minute. A shorter
/// timeout lets the watchdog restart a busy engine in the middle of a request.
const IK_HEALTH_TIMEOUT: Duration = Duration::from_secs(120);

fn health(cfg: &LlmConfig, timeout: Duration) -> Health {
    Health {
        url: format!("http://127.0.0.1:{}/health", cfg.internal_port),
        // The first start downloads the model. llama-server answers 503 while
        // it loads. ik_llama.cpp refuses connections until the model is
        // loaded. So a missing answer only counts after this.
        start_grace: Duration::from_secs(900),
        interval: Duration::from_secs(30),
        timeout,
        failures: 4,
    }
}

/// The spec of llama.cpp's llama-server.
pub async fn llamacpp_spec(
    cfg: &LlmConfig,
    slot_cache: bool,
    starts: Arc<AtomicU64>,
    restart_when_healthy: bool,
) -> ChildSpec {
    let mut env = Vec::new();
    let mut offline = false;
    if let LlmSource::HfRepo(repo) = &cfg.source {
        let _ = std::fs::create_dir_all(&cfg.hf_cache_dir);
        env.push((s("LLAMA_CACHE"), cfg.hf_cache_dir.display().to_string()));
        offline = is_cached(cfg, repo).await;
        if offline {
            tracing::info!("{repo} is cached, llama-server starts offline");
        }
    }
    let file = model_file(cfg);
    let template = template_for(cfg, file.as_deref());
    ChildSpec {
        name: s("llama-server"),
        program: cfg.server_bin.clone(),
        args: server_args(cfg, slot_cache, offline, template.as_deref()),
        env,
        // The hub checks the API key itself. llama-server listens on 127.0.0.1 only.
        env_remove: vec![s("LLAMA_API_KEY")],
        starts,
        health: Some(health(cfg, LLAMACPP_HEALTH_TIMEOUT)),
        restart_when_healthy,
    }
}

/// The spec of ik_llama.cpp's llama-server for a model file.
pub fn ik_spec(cfg: &LlmConfig, model: &Path, slot_cache: bool, threads: usize, starts: Arc<AtomicU64>) -> ChildSpec {
    let template = template_for(cfg, Some(model));
    ChildSpec {
        name: s("llama-server"),
        program: cfg.ik_bin.clone(),
        args: ik::ik_args(cfg, model, slot_cache, template.as_deref(), threads),
        env: Vec::new(),
        env_remove: vec![s("LLAMA_API_KEY")],
        starts,
        health: Some(health(cfg, IK_HEALTH_TIMEOUT)),
        restart_when_healthy: false,
    }
}

/// Builds the llama.cpp child process spec. Returns whether the prompt cache is usable.
#[cfg(test)]
pub async fn child_spec(cfg: &LlmConfig) -> (ChildSpec, bool) {
    let usable = slots_usable(cfg);
    (llamacpp_spec(cfg, usable, Default::default(), false).await, usable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn cfg(family: Family, source: LlmSource) -> LlmConfig {
        LlmConfig {
            family,
            source,
            extra_args: vec!["--n-cpu-moe".into(), "8".into()],
            ..LlmConfig::for_test()
        }
    }

    fn args(c: &LlmConfig) -> String {
        server_args(c, false, false, None).join(" ")
    }

    #[test]
    fn ling3_from_hf_args() {
        let c = cfg(
            Family::Ling3,
            LlmSource::HfRepo("inclusionAI/Ling-3.0-tiny-GGUF:Q4_K_M".into()),
        );
        let args = server_args(&c, true, true, Some(Path::new("/tmp/chat-template-x.jinja"))).join(" ");
        assert_eq!(
            args,
            "-hf inclusionAI/Ling-3.0-tiny-GGUF:Q4_K_M --no-mmproj --offline \
             --alias Ling-3.0-tiny-Q4_K_M --host 127.0.0.1 --port 8081 --no-ui \
             --device none -ngl auto -c 65536 --jinja --reasoning on --reasoning-budget 512 \
             --n-predict 1024 --slot-save-path /models/llm/slots --temp 1.0 --top-p 0.95 --top-k 20 \
             --chat-template-file /tmp/chat-template-x.jinja --n-cpu-moe 8"
        );
    }

    #[test]
    fn granite_from_file_without_reasoning() {
        let mut c = cfg(Family::Granite4, LlmSource::File("/models/llm/g.gguf".into()));
        c.reasoning = false;
        c.ctx = Ctx::K128;
        c.extra_args.clear();
        let args = server_args(&c, false, false, None).join(" ");
        assert!(args.starts_with("-m /models/llm/g.gguf --alias"));
        assert!(args.contains("-c 131072 --jinja --reasoning off --n-predict 1024 --temp 0.0"));
        assert!(!args.contains("slot-save-path"));
    }

    #[test]
    fn ling3_256k_uses_yarn() {
        let mut c = cfg(Family::Ling3, LlmSource::File("/m.gguf".into()));
        c.ctx = Ctx::K256;
        assert!(
            server_args(&c, false, false, None)
                .join(" ")
                .contains("-c 262144 --rope-scaling yarn --rope-scale 2 --yarn-orig-ctx 131072")
        );
    }

    /// The command line of every family with the settings of its model card.
    /// A refactor of the argument code must keep each line as it is.
    #[test]
    fn the_command_line_of_each_family() {
        let base = "-m /models/llm/Ling-3.0-tiny-Q4_K_M.gguf --alias Ling-3.0-tiny-Q4_K_M --host 127.0.0.1 \
                    --port 8081 --no-ui --device none -ngl auto -c 65536 --jinja";
        let rows: [(Family, bool, &str, &str); 7] = [
            (
                Family::Ling3,
                true,
                "128",
                "--reasoning on --reasoning-budget 128 --n-predict 1024 --temp 1.0 --top-p 0.95 --top-k 20",
            ),
            (
                Family::Minicpm5,
                true,
                "128",
                "--reasoning on --reasoning-budget 128 --n-predict 1024 --temp 0.9 --top-p 0.95 --min-p 0",
            ),
            (
                Family::Minicpm5,
                false,
                "256",
                "--reasoning off --n-predict 1024 --temp 0.7 --top-p 0.95 --min-p 0",
            ),
            (
                Family::Lfm3,
                true,
                "128",
                "--reasoning on --reasoning-budget 128 --n-predict 1024 --temp 0.1 --top-k 50 --repeat-penalty 1.1",
            ),
            (
                Family::Qwen35,
                true,
                "512",
                "--reasoning on --reasoning-budget 512 --n-predict 1024 --temp 1.0 --top-p 0.95 --top-k 20 \
                 --min-p 0 --presence-penalty 1.5",
            ),
            (
                Family::Qwen35,
                false,
                "512",
                "--reasoning off --n-predict 1024 --temp 0.7 --top-p 0.8 --top-k 20 --min-p 0 \
                 --presence-penalty 1.5",
            ),
            (
                Family::Granite4,
                false,
                "512",
                "--reasoning off --n-predict 1024 --temp 0.0",
            ),
        ];
        for (family, reasoning, budget, tail) in rows {
            let c = LlmConfig {
                family,
                reasoning,
                reasoning_budget: budget.into(),
                ..LlmConfig::for_test()
            };
            assert_eq!(args(&c), format!("{base} {tail}"), "{family:?} reasoning={reasoning}");
        }
    }

    #[test]
    fn context_sizes() {
        let ctx = |family, ctx| {
            let c = LlmConfig {
                family,
                ctx,
                ..LlmConfig::for_test()
            };
            args(&c)
        };
        assert!(ctx(Family::Lfm3, Ctx::K32).contains(" -c 32768 "));
        assert!(ctx(Family::Lfm3, Ctx::K64).contains(" -c 65536 "));
        assert!(ctx(Family::Lfm3, Ctx::K128).contains(" -c 131072 "));
        let lfm = ctx(Family::Lfm3, Ctx::K256);
        assert!(lfm.contains(" -c 262144 "));
        assert!(!lfm.contains("yarn"), "only Ling needs YaRN");
    }

    #[test]
    fn the_device_and_the_gpu_layers_are_passed() {
        let c = LlmConfig {
            device: "Vulkan0".into(),
            gpu_layers: "99".into(),
            extra_args: vec!["-fa".into(), "off".into()],
            ..LlmConfig::for_test()
        };
        let a = args(&c);
        assert!(a.contains("--device Vulkan0 -ngl 99 "), "{a}");
        assert!(a.ends_with(" -fa off"), "extra arguments come last: {a}");
    }

    #[test]
    fn a_zero_max_tokens_removes_the_limit() {
        let c = LlmConfig {
            max_tokens: 0,
            ..LlmConfig::for_test()
        };
        assert!(!args(&c).contains("--n-predict"));
    }

    #[test]
    fn extra_arguments_come_after_everything_else() {
        let c = LlmConfig {
            extra_args: vec!["--temp".into(), "0.2".into()],
            ..LlmConfig::for_test()
        };
        let a = server_args(&c, true, false, Some(Path::new("/t.jinja"))).join(" ");
        assert!(a.ends_with("--chat-template-file /t.jinja --temp 0.2"), "{a}");
    }

    /// Properties that hold for every combination of settings.
    #[test]
    fn invariants_over_all_settings() {
        let families = [
            Family::Ling3,
            Family::Minicpm5,
            Family::Lfm3,
            Family::Qwen35,
            Family::Granite4,
        ];
        let ctxs = [Ctx::K32, Ctx::K64, Ctx::K128, Ctx::K256];
        let sources = [
            LlmSource::File("/models/llm/x.gguf".into()),
            LlmSource::HfRepo("org/Model-GGUF:Q4_K_M".into()),
        ];
        let mut n = 0;
        for family in families {
            for reasoning in [true, false] {
                for ctx in ctxs {
                    for source in &sources {
                        for (slots, offline, template) in [(false, false, false), (true, true, true)] {
                            let c = LlmConfig {
                                family,
                                reasoning,
                                ctx,
                                source: source.clone(),
                                extra_args: vec![],
                                ..LlmConfig::for_test()
                            };
                            let tmpl = template.then(|| Path::new("/t.jinja"));
                            let v = server_args(&c, slots, offline, tmpl);
                            let a = v.join(" ");
                            let has = |s: &str| v.iter().any(|x| x == s);
                            let ctx_ =
                                format!("{family:?} {reasoning} {ctx:?} {source:?} {slots} {offline} {template}");
                            assert!(has("--jinja"), "{ctx_}");
                            assert!(has("--no-ui"), "{ctx_}");
                            assert!(a.contains("--host 127.0.0.1 --port 8081"), "{ctx_}: only loopback");
                            assert_eq!(has("--reasoning-budget"), reasoning, "{ctx_}");
                            assert!(
                                a.contains(if reasoning { "--reasoning on" } else { "--reasoning off" }),
                                "{ctx_}"
                            );
                            assert_eq!(has("--slot-save-path"), slots, "{ctx_}");
                            assert_eq!(has("--chat-template-file"), template, "{ctx_}");
                            match source {
                                LlmSource::File(_) => {
                                    assert!(a.starts_with("-m /models/llm/x.gguf "), "{ctx_}");
                                    assert!(!has("--offline") && !has("-hf"), "{ctx_}");
                                }
                                LlmSource::HfRepo(_) => {
                                    assert!(a.starts_with("-hf org/Model-GGUF:Q4_K_M --no-mmproj "), "{ctx_}");
                                    assert_eq!(has("--offline"), offline, "{ctx_}");
                                }
                            }
                            let size =
                                ["32768", "65536", "131072", "262144"][ctxs.iter().position(|x| *x == ctx).unwrap()];
                            assert!(a.contains(&format!(" -c {size} ")), "{ctx_}");
                            n += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(n, 5 * 2 * 4 * 2 * 2);
    }

    const LLAMACPP_FLAGS: &str = include_str!("../../tests/fixtures/llm/llamacpp/help_flags.txt");
    const IK_FLAGS: &str = include_str!("../../tests/fixtures/llm/ik/help_flags.txt");

    /// The options in an argument list: the words that start with `-` and are
    /// no negative numbers.
    fn options(args: &[String]) -> std::collections::BTreeSet<String> {
        args.iter()
            .filter(|a| a.starts_with('-') && a.parse::<i64>().is_err())
            .cloned()
            .collect()
    }

    /// Every option that the server passes must be in the `--help` of the engine
    /// (tests/fixtures/llm/<engine>/help_flags.txt, from
    /// scripts/record-llm-fixtures.sh). After an engine bump, a renamed or
    /// removed option fails here, before the engine fails to start.
    #[test]
    fn every_option_is_known_to_the_engine_that_gets_it() {
        let known = |list: &'static str| -> std::collections::BTreeSet<&'static str> { list.lines().collect() };
        let (llamacpp, ik) = (known(LLAMACPP_FLAGS), known(IK_FLAGS));
        assert!(
            llamacpp.len() > 100 && ik.len() > 100,
            "the recordings hold the options"
        );
        let mut used_llamacpp = std::collections::BTreeSet::new();
        let mut used_ik = std::collections::BTreeSet::new();
        for family in [
            Family::Ling3,
            Family::Minicpm5,
            Family::Lfm3,
            Family::Qwen35,
            Family::Granite4,
        ] {
            for reasoning in [true, false] {
                for ctx in [Ctx::K32, Ctx::K64, Ctx::K128, Ctx::K256] {
                    for source in [
                        LlmSource::File("/m.gguf".into()),
                        LlmSource::HfRepo("org/Model-GGUF:Q4_K_M".into()),
                    ] {
                        let c = LlmConfig {
                            family,
                            reasoning,
                            ctx,
                            source,
                            extra_args: vec![],
                            ..LlmConfig::for_test()
                        };
                        used_llamacpp.extend(options(&server_args(&c, true, true, Some(Path::new("/t.jinja")))));
                        used_ik.extend(options(&ik::ik_args(
                            &c,
                            Path::new("/m.gguf"),
                            true,
                            Some(Path::new("/t.jinja")),
                            4,
                        )));
                    }
                }
            }
        }
        for (name, used, known) in [("llama.cpp", used_llamacpp, llamacpp), ("ik_llama.cpp", used_ik, ik)] {
            let missing: Vec<_> = used.iter().filter(|o| !known.contains(o.as_str())).collect();
            assert!(missing.is_empty(), "{name} does not list {missing:?} in its --help");
            assert!(used.len() > 15, "{name}: {used:?}");
        }
    }

    #[test]
    fn finds_the_cached_model_file() {
        let dir = tempfile::tempdir().unwrap();
        let snap = dir.path().join("models--inclusionAI--Ling-3.0-tiny-GGUF/snapshots/abc");
        std::fs::create_dir_all(&snap).unwrap();
        for f in [
            "Ling-3.0-tiny-Q4_K_M.gguf",
            "Ling-3.0-tiny-Q8_0.gguf",
            "mmproj-Q4_K_M.gguf",
        ] {
            std::fs::write(snap.join(f), b"").unwrap();
        }
        let found = cached_gguf(dir.path(), "inclusionAI/Ling-3.0-tiny-GGUF:Q4_K_M").unwrap();
        assert!(found.ends_with("Ling-3.0-tiny-Q4_K_M.gguf"));
        let q8 = cached_gguf(dir.path(), "inclusionAI/Ling-3.0-tiny-GGUF:Q8_0").unwrap();
        assert!(q8.ends_with("Ling-3.0-tiny-Q8_0.gguf"));
        // Without a tag, llama.cpp takes Q4_K_M.
        assert!(cached_gguf(dir.path(), "inclusionAI/Ling-3.0-tiny-GGUF").is_some());
        assert!(cached_gguf(dir.path(), "other/repo:Q4_K_M").is_none());
    }

    #[test]
    fn cache_list_parsing() {
        assert_eq!(
            cache_key("inclusionAI/Ling-3.0-tiny-GGUF"),
            "inclusionai/ling-3.0-tiny-gguf:q4_k_m"
        );
        let out = "cached models:\n   1. inclusionai/ling-3.0-tiny-gguf:q4_k_m  (4.1 GB)\n   2. other/x:q8_0\n";
        assert!(cache_list_contains(
            out,
            &cache_key("inclusionAI/Ling-3.0-tiny-GGUF:Q4_K_M")
        ));
        assert!(!cache_list_contains(out, "other/x:q4_k_m"));
        assert!(!cache_list_contains("", "other/x:q4_k_m"));
        assert!(!cache_list_contains(
            "1 inclusionai/ling-3.0-tiny-gguf:q4_k_m",
            "inclusionai/ling-3.0-tiny-gguf:q4_k_m"
        ));
    }

    // ---- child_spec: what the hub starts and how it prepares it ----

    /// The generation prompt of the Ling template, as in the GGUF file.
    const LING_TEMPLATE: &str = "{%- if add_generation_prompt %}\n    {{- '<role>ASSISTANT</role>' }}\n    {%- if thinking_option == 'on' %}\n        {{- '\\n<think>' }}\n    {%- elif thinking_option == 'off' %}\n        {{- '\\n<think></think>' }}\n    {%- endif %}\n{%- endif %}";

    /// A stand-in for llama-server that only knows `--cache-list`.
    fn server_script(dir: &Path, name: &str, cache_list: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(
            &path,
            format!("#!/bin/sh\nif [ \"$1\" = \"--cache-list\" ]; then printf '%s' '{cache_list}'; fi\n"),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Another test thread can fork while this file is open for writing.
        // The kernel then refuses to run it (ETXTBSY) until that child execs.
        for _ in 0..100 {
            if std::process::Command::new(&path).arg("--version").output().is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        path
    }

    fn test_cfg(name: &str, dir: &Path) -> LlmConfig {
        LlmConfig {
            server_bin: server_script(dir, "llama-server", ""),
            model_name: name.into(),
            hf_cache_dir: dir.join("hf"),
            slot_dir: dir.join("slots"),
            ..LlmConfig::for_test()
        }
    }

    fn model_with_template(path: &Path, template: Option<&str>) {
        match template {
            Some(t) => crate::gguf::write_test_gguf(path, &[("general.name", "t"), ("tokenizer.chat_template", t)]),
            None => crate::gguf::write_test_gguf(path, &[("general.name", "t")]),
        }
    }

    #[tokio::test]
    async fn the_child_spec_for_a_model_file() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("m.gguf");
        model_with_template(&model, None);
        let c = LlmConfig {
            source: LlmSource::File(model.clone()),
            ..test_cfg("spec-file", dir.path())
        };
        let (spec, slots) = child_spec(&c).await;
        assert!(slots);
        assert_eq!(spec.name, "llama-server");
        assert_eq!(spec.program, c.server_bin);
        assert_eq!(
            spec.env_remove,
            vec!["LLAMA_API_KEY".to_owned()],
            "the hub checks the key"
        );
        assert!(spec.env.is_empty(), "a model file needs no download cache");
        let h = spec.health.unwrap();
        assert_eq!(h.url, "http://127.0.0.1:8081/health");
        assert_eq!(h.start_grace, Duration::from_secs(900));
        assert_eq!(h.interval, Duration::from_secs(30));
        assert_eq!(h.failures, 4);
        assert_eq!(spec.args[..2], ["-m".to_owned(), model.display().to_string()]);
    }

    #[tokio::test]
    async fn a_cached_hugging_face_model_starts_offline() {
        let dir = tempfile::tempdir().unwrap();
        let repo = "inclusionAI/Ling-3.0-tiny-GGUF:Q4_K_M";
        let c = LlmConfig {
            source: LlmSource::HfRepo(repo.into()),
            server_bin: server_script(
                dir.path(),
                "cached-server",
                "cached models:\n   1. inclusionai/ling-3.0-tiny-gguf:q4_k_m  (4.8 GB)\n",
            ),
            ..test_cfg("spec-cached", dir.path())
        };
        let (spec, _) = child_spec(&c).await;
        assert!(spec.args.contains(&"--offline".to_owned()));
        assert_eq!(
            spec.env,
            vec![("LLAMA_CACHE".to_owned(), c.hf_cache_dir.display().to_string())]
        );
        assert!(c.hf_cache_dir.is_dir(), "the download cache folder is created");
    }

    #[tokio::test]
    async fn a_model_that_is_not_cached_is_downloaded() {
        let dir = tempfile::tempdir().unwrap();
        let c = LlmConfig {
            source: LlmSource::HfRepo("inclusionAI/Ling-3.0-tiny-GGUF:Q4_K_M".into()),
            ..test_cfg("spec-download", dir.path())
        };
        let (spec, _) = child_spec(&c).await;
        assert!(!spec.args.contains(&"--offline".to_owned()));
        assert!(spec.args.contains(&"-hf".to_owned()));
        // The model is not on disk yet, so there is no template to patch.
        assert!(!spec.args.contains(&"--chat-template-file".to_owned()));
    }

    #[tokio::test]
    async fn the_template_of_the_model_file_is_patched_and_passed() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("m.gguf");
        model_with_template(&model, Some(LING_TEMPLATE));
        let c = LlmConfig {
            source: LlmSource::File(model),
            ..test_cfg("spec-template-ok", dir.path())
        };
        let (spec, _) = child_spec(&c).await;
        let at = spec
            .args
            .iter()
            .position(|a| a == "--chat-template-file")
            .expect("template flag");
        let written = std::fs::read_to_string(&spec.args[at + 1]).unwrap();
        assert!(written.contains("messages[-1].role == 'tool'"), "{written}");
        assert!(spec.args[at + 1].ends_with("chat-template-spec-template-ok.jinja"));
        let _ = std::fs::remove_file(&spec.args[at + 1]);
    }

    #[tokio::test]
    async fn no_template_flag_when_the_fix_is_off_or_cannot_apply() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("m.gguf");
        model_with_template(&model, Some(LING_TEMPLATE));
        let has_flag = |spec: &crate::supervisor::ChildSpec| spec.args.contains(&"--chat-template-file".to_owned());

        let off = LlmConfig {
            source: LlmSource::File(model.clone()),
            template_fix: false,
            ..test_cfg("spec-template-off", dir.path())
        };
        assert!(!has_flag(&child_spec(&off).await.0));

        // The patch of LFM3 does not match the Ling template: it changed upstream.
        let changed = LlmConfig {
            source: LlmSource::File(model.clone()),
            family: Family::Lfm3,
            ..test_cfg("spec-template-changed", dir.path())
        };
        assert!(!has_flag(&child_spec(&changed).await.0));

        let none = dir.path().join("none.gguf");
        model_with_template(&none, None);
        let missing = LlmConfig {
            source: LlmSource::File(none),
            ..test_cfg("spec-template-missing", dir.path())
        };
        assert!(!has_flag(&child_spec(&missing).await.0));

        let granite = LlmConfig {
            source: LlmSource::File(model),
            family: Family::Granite4,
            ..test_cfg("spec-template-granite", dir.path())
        };
        assert!(!has_flag(&child_spec(&granite).await.0), "Granite needs no patch");
    }

    #[tokio::test]
    async fn the_prompt_cache_needs_a_writable_folder() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("m.gguf");
        model_with_template(&model, None);
        let ok = LlmConfig {
            source: LlmSource::File(model.clone()),
            ..test_cfg("spec-slots-ok", dir.path())
        };
        let (spec, usable) = child_spec(&ok).await;
        assert!(usable);
        assert!(spec.args.contains(&"--slot-save-path".to_owned()));
        assert!(ok.slot_dir.is_dir());
        assert!(!ok.slot_dir.join(".write-test").exists(), "the probe file is removed");

        // A path below a regular file cannot become a folder.
        let blocked = LlmConfig {
            source: LlmSource::File(model.clone()),
            slot_dir: model.join("slots"),
            ..test_cfg("spec-slots-blocked", dir.path())
        };
        let (spec, usable) = child_spec(&blocked).await;
        assert!(!usable);
        assert!(!spec.args.contains(&"--slot-save-path".to_owned()));

        let off = LlmConfig {
            source: LlmSource::File(model),
            slot_cache: false,
            slot_dir: dir.path().join("never"),
            ..test_cfg("spec-slots-off", dir.path())
        };
        let (spec, usable) = child_spec(&off).await;
        assert!(!usable && !spec.args.contains(&"--slot-save-path".to_owned()));
        assert!(!off.slot_dir.exists(), "no folder is made when the cache is off");
    }
}
