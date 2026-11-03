//! The command line of ik_llama.cpp's llama-server.
//!
//! It takes the same values as llama.cpp, with these differences. It has no
//! `--device`, no `-ngl`, no `--offline` and no download by repository name,
//! so it gets a model file. The web interface is `--webui none`. The reply
//! limit is `-n`. A larger `max_tokens` of a client is cut to it, with a
//! warning in the log. It writes no trace file (`--log-disable`). Its own
//! default of 8 threads does not fit every CPU.

use std::path::Path;

use crate::config::LlmConfig;

use super::{ctx_args, preset, s};

pub fn ik_args(
    cfg: &LlmConfig,
    model: &Path,
    slot_cache: bool,
    template: Option<&Path>,
    threads: usize,
) -> Vec<String> {
    let mut args = vec![
        s("-m"),
        model.display().to_string(),
        s("--alias"),
        cfg.model_name.clone(),
        s("--host"),
        s("127.0.0.1"),
        s("--port"),
        cfg.internal_port.to_string(),
        s("--webui"),
        s("none"),
        // Without this flag, the server writes a trace file `llama.log` to its
        // working directory. The normal log on stderr stays.
        s("--log-disable"),
        s("-t"),
        threads.to_string(),
    ];
    args.extend(ctx_args(cfg));
    args.push(s("--jinja"));
    if cfg.reasoning {
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
        args.extend([s("-n"), cfg.max_tokens.to_string()]);
    }
    if slot_cache {
        args.extend([s("--slot-save-path"), cfg.slot_dir.display().to_string()]);
    }
    args.extend(preset(cfg));
    // Ling and LFM3 have layers without a KV cache. ik_llama.cpp can only
    // rewind such a model to a saved checkpoint. By default it saves one 5
    // tokens before the end of the prompt, which is inside the question. The
    // next question then starts from a checkpoint far back and reads about
    // 1,700 tokens again. A checkpoint 64 tokens before the end lies before
    // the question, so the next one reads about 60 tokens again.
    args.extend([s("--ctx-checkpoints-tolerance"), s("64")]);
    if let Some(t) = template {
        args.extend([s("--chat-template-file"), t.display().to_string()]);
    }
    // Last, so they override everything above.
    args.extend(cfg.extra_args.iter().cloned());
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Ctx, Family, LlmSource};

    fn args(c: &LlmConfig) -> String {
        ik_args(c, Path::new("/models/llm/m.gguf"), false, None, 6).join(" ")
    }

    #[test]
    fn ling_on_the_cpu() {
        let c = LlmConfig {
            extra_args: vec!["-fa".into(), "off".into()],
            ..LlmConfig::for_test()
        };
        let a = ik_args(
            &c,
            Path::new("/models/llm/m.gguf"),
            true,
            Some(Path::new("/t.jinja")),
            6,
        )
        .join(" ");
        assert_eq!(
            a,
            "-m /models/llm/m.gguf --alias Ling-3.0-tiny-Q4_K_M --host 127.0.0.1 --port 8081 --webui none --log-disable -t 6 \
             -c 65536 --jinja --reasoning on --reasoning-budget 512 -n 1024 --slot-save-path /models/llm/slots \
             --temp 1.0 --top-p 0.95 --top-k 20 --ctx-checkpoints-tolerance 64 --chat-template-file /t.jinja -fa off"
        );
    }

    #[test]
    fn what_ik_does_not_get() {
        for family in [Family::Ling3, Family::Minicpm5, Family::Lfm3] {
            let c = LlmConfig {
                family,
                device: "none".into(),
                source: LlmSource::HfRepo("org/Model-GGUF:Q4_K_M".into()),
                ..LlmConfig::for_test()
            };
            let a = args(&c);
            for gone in [
                "--device",
                "-ngl",
                "--offline",
                "-hf",
                "--no-mmproj",
                "--no-ui",
                "--n-predict",
            ] {
                assert!(!a.split(' ').any(|w| w == gone), "{family:?} has {gone}: {a}");
            }
            assert!(a.starts_with("-m /models/llm/m.gguf "), "{a}");
        }
    }

    #[test]
    fn the_settings_of_the_family_are_the_same_as_for_llamacpp() {
        for family in [Family::Ling3, Family::Minicpm5, Family::Lfm3] {
            for reasoning in [true, false] {
                let c = LlmConfig {
                    family,
                    reasoning,
                    reasoning_budget: "128".into(),
                    ..LlmConfig::for_test()
                };
                let ik = args(&c);
                let main = crate::llm::server_args(&c, false, false, None).join(" ");
                for part in [
                    "--jinja",
                    if reasoning {
                        "--reasoning on --reasoning-budget 128"
                    } else {
                        "--reasoning off"
                    },
                    "-c 65536",
                ] {
                    assert!(ik.contains(part) && main.contains(part), "{family:?}: {part}");
                }
                // The sampling values follow the model card on both engines.
                let tail = |s: &str| {
                    s.split("--temp")
                        .nth(1)
                        .map(|t| t.replace(" --ctx-checkpoints-tolerance 64", ""))
                };
                assert_eq!(tail(&ik), tail(&main), "{family:?}");
            }
        }
    }

    #[test]
    fn context_sizes_and_the_reply_limit() {
        let ling = |ctx, max_tokens| LlmConfig {
            ctx,
            max_tokens,
            ..LlmConfig::for_test()
        };
        assert!(args(&ling(Ctx::K32, 1024)).contains(" -c 32768 "));
        assert!(
            args(&ling(Ctx::K256, 1024))
                .contains("-c 262144 --rope-scaling yarn --rope-scale 2 --yarn-orig-ctx 131072")
        );
        assert!(!args(&ling(Ctx::K64, 0)).contains(" -n "));
        assert!(args(&ling(Ctx::K64, 77)).contains(" -n 77"));
    }

    #[test]
    fn extra_arguments_come_last_and_the_threads_are_set() {
        let c = LlmConfig {
            extra_args: vec!["-t".into(), "3".into()],
            ..LlmConfig::for_test()
        };
        let a = args(&c);
        assert!(a.contains(" -t 6 "), "the default thread count: {a}");
        assert!(a.ends_with(" -t 3"), "the user value wins: {a}");
    }
}
