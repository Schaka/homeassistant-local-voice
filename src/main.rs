//! assistant-hub: one server on one port for a local Home Assistant voice setup.
//!
//! It serves Wyoming STT and TTS, proxies llama-server under `/llm`, offers MCP
//! tools on `/mcp`, and answers web and YouTube searches. The native engines
//! (audiocpp_server, llama-server) run as child processes on 127.0.0.1.

mod audio;
mod config;
mod denoise;
mod gguf;
mod gpu;
mod jellyfin;
mod llm;
mod mcp;
mod media;
mod probe;
mod search;
mod server;
mod stt;
mod supervisor;
mod text;
mod tts;
mod wyoming;
mod youtube;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::sync::CancellationToken;

use crate::config::{Config, LlmSource};
use crate::search::SearchBackend;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server (the default).
    Serve,
    /// Check GGUF model files for truncation.
    VerifyGguf { files: Vec<PathBuf> },
    /// Exit 0 if the server on ADDR answers GET /health.
    Healthcheck {
        #[arg(long, default_value = "127.0.0.1:10300")]
        addr: String,
    },
    /// Print the command line of the LLM engine for the current environment
    /// as JSON, and prepare its files (chat template, cache folder). Scripts
    /// use it to start the engine exactly as the server does.
    #[command(name = "llm-command")]
    LlmEngine,
    /// Evaluate a conversation agent with Home Assistant style questions.
    Eval {
        #[command(subcommand)]
        what: EvalCmd,
    },
    /// Talk to a running server.
    Probe {
        #[arg(long, default_value = "127.0.0.1:10300")]
        addr: String,
        #[command(subcommand)]
        what: Probe,
    },
}

#[derive(Subcommand)]
enum EvalCmd {
    /// Ask the questions of src/llm/assist.json and score the answers.
    Llm {
        /// The hub. The questions go to http://ADDR/llm/v1.
        #[arg(long, default_value = "127.0.0.1:10300")]
        addr: String,
        /// An OpenAI compatible /v1 URL instead of the hub, for example an
        /// engine on its own port.
        #[arg(long)]
        base_url: Option<String>,
        /// Bearer token. Only needed with LLM_API_KEY.
        #[arg(long)]
        key: Option<String>,
        /// Times to ask each question.
        #[arg(long, default_value_t = 3)]
        repeat: usize,
        /// The values that Home Assistant sends. Set the ones of the model.
        #[arg(long, default_value_t = 0.7)]
        temperature: f64,
        #[arg(long, default_value_t = 1.0)]
        top_p: f64,
        /// Ask only these question ids.
        #[arg(long)]
        only: Vec<String>,
        /// Seconds to wait for one answer.
        #[arg(long, default_value_t = 300)]
        timeout: u64,
        /// Write the full report as JSON.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Decide whether engine B may replace engine A: it must answer as well
    /// and be faster. Exit code 1 if not.
    Compare { a: PathBuf, b: PathBuf },
}

#[derive(Subcommand)]
enum Probe {
    /// Print the Wyoming info.
    Describe,
    /// Transcribe a WAV file.
    Stt { wav: PathBuf },
    /// Synthesize TEXT into a WAV file.
    Tts { text: String, out: PathBuf },
    /// Send one chat request with a Home Assistant style tool to /llm/v1 and
    /// print the reply, the tool calls and the speed.
    Llm {
        prompt: String,
        /// Extra fake entities (about 40 tokens each), so the prompt is as
        /// long as a real Assist prompt. 100 gives about 4,000 tokens.
        #[arg(long, default_value_t = 0)]
        devices: usize,
        /// Bearer token. Only needed with LLM_API_KEY.
        #[arg(long)]
        key: Option<String>,
    },
}

fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .with_timer(tracing_subscriber::fmt::time::ChronoLocal::rfc_3339())
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread().enable_all().build()?)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve(),
        Command::VerifyGguf { files } => {
            return if files.iter().all(|f| gguf::check(f)) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
        Command::Healthcheck { addr } => runtime().and_then(|rt| rt.block_on(probe::healthcheck(&addr))),
        Command::LlmEngine => runtime().and_then(|rt| rt.block_on(print_llm_command())),
        Command::Eval { what } => return run_eval(what),
        Command::Probe { addr, what } => runtime().and_then(|rt| {
            rt.block_on(async {
                match what {
                    Probe::Describe => probe::describe(&addr).await,
                    Probe::Stt { wav } => probe::stt(&addr, &wav).await,
                    Probe::Tts { text, out } => probe::tts(&addr, &text, &out).await,
                    Probe::Llm { prompt, devices, key } => probe::llm(&addr, &prompt, devices, key.as_deref()).await,
                }
            })
        }),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn print_llm_command() -> Result<()> {
    let cfg = Config::from_env()?;
    let llm_cfg = cfg
        .llm
        .context("no LLM is configured (LLM_HF_REPO or LLM_MODEL_FILENAME)")?;
    let mut setup = llm::specs::setup(&llm_cfg);
    let (decision, spec) = setup.specs.plan(None).await;
    println!(
        "{}",
        serde_json::json!({
            "engine": decision.kind.label(),
            "reason": decision.reason,
            "program": spec.program,
            "args": spec.args,
            "env": spec.env,
            "env_remove": spec.env_remove,
            "port": llm_cfg.internal_port,
        })
    );
    Ok(())
}

fn run_eval(what: EvalCmd) -> ExitCode {
    let result = match what {
        EvalCmd::Llm {
            addr,
            base_url,
            key,
            repeat,
            temperature,
            top_p,
            only,
            timeout,
            out,
        } => runtime().and_then(|rt| {
            rt.block_on(async {
                let fx = llm::eval::Fixture::builtin()?;
                let opts = llm::eval::RunOptions {
                    base_url: base_url.unwrap_or_else(|| format!("http://{addr}/llm/v1")),
                    key,
                    repeat,
                    temperature,
                    top_p,
                    only,
                    timeout: std::time::Duration::from_secs(timeout),
                };
                let report = llm::eval::run(&fx, &opts).await?;
                llm::eval::print_summary(&report);
                if let Some(path) = out {
                    std::fs::write(&path, serde_json::to_string_pretty(&report)?)?;
                    println!("report: {}", path.display());
                }
                Ok(true)
            })
        }),
        EvalCmd::Compare { a, b } => (|| {
            let read =
                |p: &PathBuf| -> Result<llm::eval::Report> { Ok(serde_json::from_str(&std::fs::read_to_string(p)?)?) };
            let verdict = llm::eval::compare(&read(&a)?, &read(&b)?, llm::eval::Gate::default());
            for line in &verdict.lines {
                println!("{line}");
            }
            println!(
                "{}",
                if verdict.pass {
                    "B may replace A"
                } else {
                    "B may not replace A"
                }
            );
            Ok(verdict.pass)
        })(),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn serve() -> Result<()> {
    init_logging();
    let mut cfg = Config::from_env()?;
    choose_speech_devices(&mut cfg);

    // Reject a truncated model before it reaches a native loader.
    let mut models = vec![cfg.stt.model.clone(), cfg.tts.dir.join(&cfg.tts.model_filename)];
    if let Some(llm) = &cfg.llm
        && let LlmSource::File(path) = &llm.source
    {
        models.push(path.clone());
    }
    if !models.iter().all(|m| gguf::check(m)) {
        anyhow::bail!("a model file failed the integrity check (see above), refusing to start");
    }

    // Environment for the native libraries. Set here, before the runtime
    // starts any thread, because changing the environment is not thread-safe.
    let mut env_vars = vec![("PARAKEET_DEVICE", cfg.stt.device.clone().unwrap_or_default())];
    if let Some(search) = &cfg.search {
        env_vars.push(search::prepare(search)?);
    }
    for (key, value) in env_vars {
        // SAFETY: no other thread exists yet (the tokio runtime starts below).
        unsafe { std::env::set_var(key, value) };
    }

    let gpu_lost = runtime()?.block_on(run(cfg))?;
    if gpu_lost {
        anyhow::bail!("GPU lost, exiting so that the container restarts");
    }
    Ok(())
}

/// Picks the devices for STT and TTS. Settings that the user made stay as
/// they are. Without a Vulkan GPU, both run on the CPU: llvmpipe is no option,
/// because ggml does not use it, and audio.cpp fails on a missing Vulkan device.
fn choose_speech_devices(cfg: &mut Config) {
    let gpus = gpu::vulkan_devices(&cfg.llama_server);
    let on_gpu = !gpus.is_empty();
    if on_gpu {
        tracing::info!("Vulkan GPUs: {}", gpus.join(", "));
    } else {
        tracing::info!("no Vulkan GPU found, STT and TTS run on the CPU");
    }
    if cfg.stt.device.is_none() {
        cfg.stt.device = Some(if on_gpu { gpus[0].clone() } else { "cpu".to_owned() });
    }
    if cfg.tts.backend == config::TtsBackend::Auto {
        cfg.tts.backend = if on_gpu {
            config::TtsBackend::Vulkan
        } else {
            config::TtsBackend::Cpu
        };
    }
    if cfg.tts.threads.is_none() {
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
        let threads = if cfg.tts.backend == config::TtsBackend::Cpu {
            cores.min(8)
        } else {
            2
        };
        cfg.tts.threads = Some(u32::try_from(threads).unwrap_or(2));
    }
    tracing::info!(
        "STT device {}, TTS backend {} with {} threads",
        cfg.stt.device.as_deref().unwrap_or(""),
        cfg.tts.backend.name(),
        cfg.tts.threads.unwrap_or(2)
    );
}

/// Runs until SIGTERM or SIGINT. Returns true if it stopped because the GPU
/// was lost.
async fn run(cfg: Config) -> Result<bool> {
    let children = CancellationToken::new();
    let shutdown = CancellationToken::new();
    let gpu_lost = CancellationToken::new();
    let mut supervised = Vec::new();
    // choose_speech_devices sets both before run.
    let stt_on_gpu = cfg
        .stt
        .device
        .as_deref()
        .is_some_and(|device| !device.eq_ignore_ascii_case("cpu"));
    let tts_on_gpu = cfg.tts.backend == config::TtsBackend::Vulkan;

    // TTS engine.
    supervised.push(supervisor::spawn(tts::child_spec(&cfg.tts)?, children.clone()));
    let tts: Arc<dyn tts::TextToSpeech> = Arc::new(tts::AudioCpp::new(&cfg.tts)?);

    // LLM engine and its prompt cache.
    let mut slot_cache = None;
    let llm_proxy = match &cfg.llm {
        Some(llm_cfg) => {
            let llm::specs::Setup {
                specs,
                slots_usable,
                active,
                starts,
            } = llm::specs::setup(llm_cfg);
            supervised.push(supervisor::spawn_source(specs, children.clone()));
            if slots_usable {
                let cache = llm::slots::SlotCache::new(
                    llm_cfg.internal_port,
                    llm_cfg.slot_dir.clone(),
                    llm_cfg.slot_file(),
                    starts,
                    Duration::from_secs(5),
                )?
                .per_engine(active, llm_cfg.slot_file_for(llm::engine::EngineKind::Ik));
                tokio::spawn(cache.clone().run(shutdown.clone()));
                slot_cache = Some(cache);
            }
            Some(
                llm::proxy::LlmProxy::new(llm_cfg.internal_port, llm_cfg.api_key.clone())?
                    .with_retry_empty(llm_cfg.reasoning && llm_cfg.retry_empty),
            )
        }
        None => None,
    };

    // STT engine, in this process.
    let stt_cfg = cfg.stt.clone();
    let stt: Option<Arc<dyn stt::SpeechToText>> =
        match tokio::task::spawn_blocking(move || stt::Parakeet::load(&stt_cfg.lib, &stt_cfg.model)).await? {
            Ok(p) => Some(Arc::new(p)),
            Err(err) => {
                tracing::error!("STT is not available: {err:#}");
                None
            }
        };

    // Optional cleanup of the STT audio (Hush), in this process on the CPU.
    // A failure leaves STT on the raw audio, it never stops the server.
    let denoise: Option<Arc<dyn denoise::Denoise>> = match cfg.stt.denoise.clone() {
        Some(d) => {
            match tokio::task::spawn_blocking(move || denoise::Hush::load(&d.lib, &d.model, d.atten_lim_db)).await? {
                Ok(h) => Some(Arc::new(h)),
                Err(err) => {
                    tracing::error!("STT cleanup is not available, STT hears the raw audio: {err:#}");
                    None
                }
            }
        }
        None => None,
    };

    // Web search, SearXNG in this process.
    let web_search: Option<Arc<dyn SearchBackend>> = match cfg.search.clone() {
        Some(search_cfg) => match tokio::task::spawn_blocking(move || search::SearXng::load(&search_cfg)).await? {
            Ok(s) => Some(Arc::new(search::SearXngBackend(Arc::new(s)))),
            Err(err) => {
                tracing::error!("web search is not available: {err:#}");
                None
            }
        },
        None => None,
    };

    let youtube = match cfg.youtube.clone() {
        Some(y) => Some(Arc::new(youtube::YouTube::new(y)?)),
        None => None,
    };

    let jellyfin = match cfg.jellyfin.clone() {
        Some(j) => Some(Arc::new(jellyfin::Jellyfin::new(j)?)),
        None => None,
    };
    // The media index loads its model and syncs in the background.
    let media = match (cfg.media.clone(), &jellyfin) {
        (Some(m), Some(jf)) => {
            let index = media::MediaIndex::new(m, jf.clone());
            jf.attach_media(index.clone());
            tokio::spawn(index.clone().run(shutdown.clone()));
            Some(index)
        }
        _ => None,
    };

    let voice = Arc::new(wyoming::Voice {
        stt,
        denoise,
        tts,
        gpu: gpu::Gpu::new(gpu_lost.clone(), stt_on_gpu, tts_on_gpu),
        info: wyoming::info(&cfg.stt.model_name, &cfg.tts.voice_id, &cfg.tts.languages_bcp),
        debug_dir: cfg.stt.debug_dir.clone(),
    });
    let state = Arc::new(server::AppState {
        voice,
        llm: llm_proxy,
        tools: mcp::Tools {
            web_search: web_search.clone(),
        },
        web_search,
        youtube,
        jellyfin,
        media,
    });

    let listener = tokio::net::TcpListener::bind(cfg.listen)
        .await
        .with_context(|| format!("cannot listen on {}", cfg.listen))?;
    tracing::info!(
        "assistant-hub {} listening on {}",
        env!("CARGO_PKG_VERSION"),
        cfg.listen
    );
    let server = tokio::spawn(server::run(listener, state, shutdown.clone()));

    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = term.recv() => tracing::info!("SIGTERM, stopping"),
        _ = int.recv() => tracing::info!("SIGINT, stopping"),
        () = gpu_lost.cancelled() => {}
    }

    shutdown.cancel();
    // Save the prompt cache while llama-server still runs (about 0.3 s).
    if let Some(cache) = slot_cache {
        let _ = tokio::time::timeout(Duration::from_secs(8), cache.save_now()).await;
    }
    children.cancel();
    for task in supervised {
        let _ = tokio::time::timeout(Duration::from_secs(8), task).await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
    Ok(gpu_lost.is_cancelled())
}
