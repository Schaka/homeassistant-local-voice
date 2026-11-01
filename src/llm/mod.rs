//! The optional conversation agent: llama-server as a child process on an
//! internal port, reached through `/llm/...` on the main port.

pub mod proxy;
pub mod slots;

use std::path::Path;
use std::process::Stdio;

use crate::config::{Ctx, Family, LlmConfig, LlmSource};
use crate::supervisor::ChildSpec;

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
    let mut out: Vec<String> = v.iter().map(|x| s(x)).collect();
    if cfg.family == Family::Ling3 && cfg.template_fix {
        // After a tool result the turn starts with a closed <think></think>, so
        // the answer is never filed as reasoning (templates/ling3.jinja).
        out.push(s("--chat-template-file"));
        out.push(cfg.templates_dir.join("ling3.jinja").display().to_string());
    }
    out
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
/// upstream file gets a new ETag.
pub fn server_args(cfg: &LlmConfig, slot_cache: bool, offline: bool) -> Vec<String> {
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
    if slot_cache {
        args.extend([s("--slot-save-path"), cfg.slot_dir.display().to_string()]);
    }
    args.extend(preset(cfg));
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
    let spec = ChildSpec {
        name: s("llama-server"),
        program: cfg.server_bin.clone(),
        args: server_args(cfg, slot_cache, offline),
        env,
        // The hub checks the API key itself. llama-server listens on 127.0.0.1 only.
        env_remove: vec![s("LLAMA_API_KEY")],
        starts: Default::default(),
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
            api_key: None,
            extra_args: vec!["--n-cpu-moe".into(), "8".into()],
            slot_cache: true,
            template_fix: true,
            internal_port: 8081,
            hf_cache_dir: PathBuf::from("/models/llm/hf"),
            slot_dir: PathBuf::from("/models/llm/slots"),
            templates_dir: PathBuf::from("/app/templates"),
        }
    }

    #[test]
    fn ling3_from_hf_matches_the_old_entrypoint() {
        let c = cfg(
            Family::Ling3,
            LlmSource::HfRepo("inclusionAI/Ling-3.0-tiny-GGUF:Q4_K_M".into()),
        );
        let args = server_args(&c, true, true).join(" ");
        assert_eq!(
            args,
            "-hf inclusionAI/Ling-3.0-tiny-GGUF:Q4_K_M --no-mmproj --offline \
             --alias Ling-3.0-tiny-Q4_K_M --host 127.0.0.1 --port 8081 --no-ui \
             --device none -ngl auto -c 65536 --jinja --reasoning on --reasoning-budget 512 \
             --slot-save-path /models/llm/slots --temp 1.0 --top-p 0.95 --top-k 20 \
             --chat-template-file /app/templates/ling3.jinja --n-cpu-moe 8"
        );
    }

    #[test]
    fn granite_from_file_without_reasoning() {
        let mut c = cfg(Family::Granite4, LlmSource::File("/models/llm/g.gguf".into()));
        c.reasoning = false;
        c.ctx = Ctx::K128;
        c.extra_args.clear();
        let args = server_args(&c, false, false).join(" ");
        assert!(args.starts_with("-m /models/llm/g.gguf --alias"));
        assert!(args.contains("-c 131072 --jinja --reasoning off --temp 0.0"));
        assert!(!args.contains("slot-save-path"));
    }

    #[test]
    fn ling3_256k_uses_yarn() {
        let mut c = cfg(Family::Ling3, LlmSource::File("/m.gguf".into()));
        c.ctx = Ctx::K256;
        assert!(
            server_args(&c, false, false)
                .join(" ")
                .contains("-c 262144 --rope-scaling yarn --rope-scale 2 --yarn-orig-ctx 131072")
        );
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
