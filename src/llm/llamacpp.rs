//! The command line of llama.cpp's llama-server.

use std::path::Path;

use crate::config::{LlmConfig, LlmSource};

use super::{ctx_args, preset, s};

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
