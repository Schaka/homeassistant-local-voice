//! The optional conversation agent: llama-server as a child process on an
//! internal port, reached through `/llm/...` on the main port.

pub mod proxy;
pub mod slots;
pub mod template;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::config::{Ctx, Family, LlmConfig, LlmSource};
use crate::supervisor::{ChildSpec, Health};

fn s(v: &str) -> String {
    v.to_owned()
}

/// Sampling values and chat template per family. They follow each model card.
/// Home Assistant sends its own temperature and top_p with each request, so
/// set the same values in its agent options (docs/llm.md).
fn preset(cfg: &LlmConfig) -> Vec<String> {
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

fn ctx_args(cfg: &LlmConfig) -> Vec<String> {
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

/// The full llama-server command line. `offline` adds `--offline` for a
/// Hugging Face model that is already in the cache: once cached, the server
/// never goes online again, because llama.cpp deletes the cached copy when the
/// upstream file gets a new ETag. `template` is a patched chat template
/// ([`template`]).
pub fn server_args(cfg: &LlmConfig, slot_cache: bool, offline: bool, template: Option<&Path>) -> Vec<String> {
    let mut args = match &cfg.source {
        LlmSource::File(path) => vec![s("-m"), path.display().to_string()],
        LlmSource::HfRepo(repo) => {
            let mut a = vec![s("-hf"), repo.clone(), s("--no-mmproj")];
            if offline {
                a.push(s("--offline"));
            }
            a
        }
    };
    args.extend([
        s("--alias"),
        cfg.model_name.clone(),
        s("--host"),
        s("127.0.0.1"),
        s("--port"),
        cfg.internal_port.to_string(),
        s("--no-ui"),
        s("--device"),
        cfg.device.clone(),
        s("-ngl"),
        cfg.gpu_layers.clone(),
    ]);
    args.extend(ctx_args(cfg));
    args.push(s("--jinja"));
    if cfg.reasoning {
        // A small model can think for 1500+ tokens on one command, so thinking
        // is capped. llama-server then forces the answer.
        args.extend([
            s("--reasoning"),
            s("on"),
            s("--reasoning-budget"),
            cfg.reasoning_budget.clone(),
        ]);
    } else {
        args.extend([s("--reasoning"), s("off")]);
    }
    if cfg.max_tokens > 0 {
        // A broken chat template or a tool-call loop must not generate until
        // the whole context is full. That holds the only slot for a long time.
        // A client that sends `max_tokens` overrides this default.
        args.extend([s("--n-predict"), cfg.max_tokens.to_string()]);
    }
    if slot_cache {
        args.extend([s("--slot-save-path"), cfg.slot_dir.display().to_string()]);
    }
    args.extend(preset(cfg));
    if let Some(t) = template {
        args.extend([s("--chat-template-file"), t.display().to_string()]);
    }
    // Last, so they override everything above.
    args.extend(cfg.extra_args.iter().cloned());
    args
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

/// Builds the child process spec. Returns whether the prompt cache is usable.
pub async fn child_spec(cfg: &LlmConfig) -> (ChildSpec, bool) {
    let slot_cache = cfg.slot_cache && {
        let ok = writable_dir(&cfg.slot_dir);
        if !ok {
            tracing::warn!(
                "{} is not writable, the prompt cache is not kept across restarts",
                cfg.slot_dir.display()
            );
        }
        ok
    };
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
    let model_file = match &cfg.source {
        LlmSource::File(path) => Some(path.clone()),
        LlmSource::HfRepo(repo) => cached_gguf(&cfg.hf_cache_dir, repo),
    };
    let template = match (&model_file, cfg.template_fix) {
        (Some(file), true) => patched_template(cfg, file),
        (None, true) => {
            tracing::info!("the model is not downloaded yet: the chat template fix applies from the next start");
            None
        }
        (_, false) => None,
    };
    let spec = ChildSpec {
        name: s("llama-server"),
        program: cfg.server_bin.clone(),
        args: server_args(cfg, slot_cache, offline, template.as_deref()),
        env,
        // The hub checks the API key itself. llama-server listens on 127.0.0.1 only.
        env_remove: vec![s("LLAMA_API_KEY")],
        starts: Default::default(),
        health: Some(Health {
            url: format!("http://127.0.0.1:{}/health", cfg.internal_port),
            // The first start downloads the model. llama-server answers 503
            // while it loads, so a missing answer only counts after this.
            start_grace: Duration::from_secs(900),
            interval: Duration::from_secs(30),
            timeout: Duration::from_secs(10),
            failures: 4,
        }),
    };
    (spec, slot_cache)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cfg(family: Family, source: LlmSource) -> LlmConfig {
        LlmConfig {
            server_bin: PathBuf::from("/opt/llama.cpp/llama-server"),
            source,
            model_name: "Ling-3.0-tiny-Q4_K_M".into(),
            family,
            device: "none".into(),
            gpu_layers: "auto".into(),
            ctx: Ctx::K64,
            reasoning: true,
            reasoning_budget: "512".into(),
            max_tokens: 1024,
            api_key: None,
            extra_args: vec!["--n-cpu-moe".into(), "8".into()],
            slot_cache: true,
            template_fix: true,
            internal_port: 8081,
            hf_cache_dir: PathBuf::from("/models/llm/hf"),
            slot_dir: PathBuf::from("/models/llm/slots"),
        }
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
    }
}
