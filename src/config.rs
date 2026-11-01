//! Configuration from environment variables.
//!
//! `docs/configuration.md` lists them all.

use std::env;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone)]
pub struct Config {
    pub listen: SocketAddr,
    /// llama-server binary. It also lists the Vulkan GPUs at start (gpu.rs).
    pub llama_server: PathBuf,
    pub stt: SttConfig,
    pub tts: TtsConfig,
    pub llm: Option<LlmConfig>,
    pub search: Option<SearchConfig>,
    pub youtube: Option<YoutubeConfig>,
    pub jellyfin: Option<JellyfinConfig>,
    pub media: Option<crate::media::MediaConfig>,
}

#[derive(Debug, Clone)]
pub struct SttConfig {
    pub lib: PathBuf,
    pub model: PathBuf,
    pub model_name: String,
    /// `PARAKEET_DEVICE`. None: the first Vulkan GPU, or the CPU without one.
    pub device: Option<String>,
    pub debug_dir: Option<PathBuf>,
}

/// Where TTS runs. `Auto` picks Vulkan when a Vulkan GPU exists, else the CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtsBackend {
    Auto,
    Vulkan,
    Cpu,
}

impl TtsBackend {
    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "auto" => Self::Auto,
            "vulkan" => Self::Vulkan,
            "cpu" => Self::Cpu,
            other => bail!("unknown TTS_BACKEND={other} (expected auto, vulkan or cpu)"),
        })
    }

    /// The value for the audiocpp_server configuration. `Auto` must be resolved first.
    pub fn name(self) -> &'static str {
        match self {
            Self::Vulkan | Self::Auto => "vulkan",
            Self::Cpu => "cpu",
        }
    }
}

#[derive(Debug, Clone)]
pub struct TtsConfig {
    pub server_bin: PathBuf,
    pub dir: PathBuf,
    pub model_filename: String,
    pub model_id: String,
    pub language: String,
    pub voice_id: String,
    pub languages_bcp: Vec<String>,
    pub backend: TtsBackend,
    pub vk_device: u32,
    /// `TTS_THREADS`. None: 2 on a GPU, up to 8 on the CPU.
    pub threads: Option<u32>,
    pub internal_port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmSource {
    File(PathBuf),
    HfRepo(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Ling3,
    Minicpm5,
    Lfm3,
    Qwen35,
    Granite4,
}

impl Family {
    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "ling3" => Self::Ling3,
            "minicpm5" => Self::Minicpm5,
            "lfm3" => Self::Lfm3,
            "qwen35" => Self::Qwen35,
            "granite4" => Self::Granite4,
            other => bail!("unknown LLM_FAMILY={other} (expected ling3, minicpm5, lfm3, qwen35 or granite4)"),
        })
    }

    /// The family from a model file or repository name.
    pub fn detect(name: &str) -> Option<Self> {
        let name = name.to_lowercase();
        if name.contains("ling-3") {
            Some(Self::Ling3)
        } else if name.contains("minicpm5") {
            Some(Self::Minicpm5)
        } else if name.contains("lfm3-") {
            Some(Self::Lfm3)
        } else if name.contains("qwen3.5") || name.contains("qwen3_5") {
            Some(Self::Qwen35)
        } else if name.contains("granite-4") {
            Some(Self::Granite4)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ctx {
    K32,
    K64,
    K128,
    K256,
}

impl Ctx {
    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "32k" => Self::K32,
            "64k" => Self::K64,
            "128k" => Self::K128,
            "256k" => Self::K256,
            other => bail!("unknown LLM_CTX={other} (expected 32k, 64k, 128k or 256k)"),
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::K32 => "32k",
            Self::K64 => "64k",
            Self::K128 => "128k",
            Self::K256 => "256k",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub server_bin: PathBuf,
    pub source: LlmSource,
    pub model_name: String,
    pub family: Family,
    pub device: String,
    pub gpu_layers: String,
    pub ctx: Ctx,
    pub reasoning: bool,
    pub reasoning_budget: String,
    pub api_key: Option<String>,
    pub extra_args: Vec<String>,
    pub slot_cache: bool,
    pub template_fix: bool,
    pub internal_port: u16,
    pub hf_cache_dir: PathBuf,
    pub slot_dir: PathBuf,
    pub templates_dir: PathBuf,
}

impl LlmConfig {
    /// File name of the saved prompt cache. It holds the model and the context
    /// size, because a cache of another model or size cannot be restored.
    pub fn slot_file(&self) -> String {
        format!("{}-{}.slot", self.model_name, self.ctx.label())
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct SearchConfig {
    pub language: String,
    pub python_paths: Vec<PathBuf>,
    pub settings_path: PathBuf,
}

#[derive(Clone)]
pub struct JellyfinConfig {
    pub url: String,
    pub api_key: String,
    /// The user whose "next up" list counts. A request can name another one.
    pub user: String,
}

// The API key stays out of debug output.
impl std::fmt::Debug for JellyfinConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JellyfinConfig")
            .field("url", &self.url)
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct YoutubeConfig {
    pub language: String,
    pub region: String,
    pub client_version: String,
}

fn var(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

fn var_or(name: &str, default: &str) -> String {
    var(name).unwrap_or_else(|| default.to_owned())
}

fn on_off(name: &str, default: bool) -> Result<bool> {
    match var(name).as_deref() {
        None => Ok(default),
        Some("on") => Ok(true),
        Some("off") => Ok(false),
        Some(other) => bail!("unknown {name}={other} (expected on or off)"),
    }
}

fn parse_num<T: std::str::FromStr>(name: &str, default: T) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    match var(name) {
        None => Ok(default),
        Some(v) => v.parse().map_err(|e| anyhow::anyhow!("invalid {name}={v}: {e}")),
    }
}

/// `inclusionAI/Ling-3.0-tiny-GGUF:Q4_K_M` becomes `Ling-3.0-tiny-Q4_K_M`.
pub fn model_name_from_repo(repo: &str) -> String {
    let repo = repo.rsplit('/').next().unwrap_or(repo);
    let (name, tag) = match repo.split_once(':') {
        Some((name, tag)) => (name, Some(tag)),
        None => (repo, None),
    };
    let name = name.strip_suffix("-GGUF").unwrap_or(name);
    match tag {
        Some(tag) => format!("{name}-{tag}"),
        None => name.to_owned(),
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let bind: IpAddr = var_or("BIND_ADDRESS", "0.0.0.0")
            .parse()
            .context("invalid BIND_ADDRESS")?;
        let listen = SocketAddr::new(bind, parse_num("PORT", 10300u16)?);

        let stt = SttConfig {
            lib: PathBuf::from(var_or("STT_LIB", "/usr/lib/libparakeet.so")),
            model: PathBuf::from(var_or("STT_DIR", "/models/stt"))
                .join(var_or("STT_MODEL_FILENAME", "parakeet-ctc-0.6b-q8_0.gguf")),
            model_name: var_or("STT_MODEL_NAME", "parakeet-ctc-0.6b"),
            device: var("PARAKEET_DEVICE"),
            debug_dir: var("STT_DEBUG_DIR").map(PathBuf::from),
        };

        let tts = TtsConfig {
            server_bin: PathBuf::from(var_or("AUDIOCPP_SERVER", "/usr/local/bin/audiocpp_server")),
            dir: PathBuf::from(var_or("TTS_DIR", "/models/pocket-tts")),
            model_filename: var_or("TTS_MODEL_FILENAME", "pocket-tts-english-q8_0.gguf"),
            model_id: var_or("TTS_MODEL_ID", "pocket-tts"),
            language: var_or("TTS_LANGUAGE", "english"),
            voice_id: var_or("TTS_VOICE_ID", "alba"),
            languages_bcp: var_or("TTS_LANGUAGES_BCP", "en")
                .split(',')
                .map(|l| l.trim().to_owned())
                .filter(|l| !l.is_empty())
                .collect(),
            backend: TtsBackend::parse(&var_or("TTS_BACKEND", "auto"))?,
            vk_device: parse_num("VK_DEVICE", 0)?,
            threads: var("TTS_THREADS")
                .map(|v| v.parse().map_err(|e| anyhow::anyhow!("invalid TTS_THREADS={v}: {e}")))
                .transpose()?,
            internal_port: 8100,
        };

        Ok(Self {
            listen,
            llama_server: PathBuf::from(var_or("LLAMA_SERVER", "/opt/llama.cpp/llama-server")),
            stt,
            tts,
            llm: Self::llm_from_env()?,
            search: Self::search_from_env()?,
            youtube: Self::youtube_from_env()?,
            jellyfin: Self::jellyfin_from_env()?,
            media: Self::media_from_env()?,
        })
    }

    fn llm_from_env() -> Result<Option<LlmConfig>> {
        let file = var("LLM_MODEL_FILENAME");
        let repo = var("LLM_HF_REPO");
        let llm_dir = PathBuf::from(var_or("LLM_DIR", "/models/llm"));
        let (source, default_name, detect_from) = match (file, repo) {
            (Some(_), Some(_)) => bail!("set LLM_MODEL_FILENAME or LLM_HF_REPO, not both"),
            (None, None) => return Ok(None),
            (Some(file), None) => {
                let name = file.strip_suffix(".gguf").unwrap_or(&file).to_owned();
                (LlmSource::File(llm_dir.join(&file)), name, file)
            }
            (None, Some(repo)) => {
                let name = model_name_from_repo(&repo);
                (LlmSource::HfRepo(repo.clone()), name, repo)
            }
        };
        let family = match var("LLM_FAMILY") {
            Some(f) => Family::parse(&f)?,
            None => Family::detect(&detect_from).with_context(|| {
                format!("set LLM_FAMILY (ling3, minicpm5, lfm3, qwen35 or granite4) for {detect_from}")
            })?,
        };
        let ctx = Ctx::parse(&var_or("LLM_CTX", "64k"))?;
        if ctx == Ctx::K256 && !matches!(family, Family::Ling3 | Family::Lfm3) {
            bail!("LLM_CTX=256k is only supported for LLM_FAMILY=ling3 or lfm3 (others: max 128k)");
        }
        let reasoning = match var_or("LLM_REASONING", "on").as_str() {
            "on" => family != Family::Granite4,
            "off" => false,
            other => bail!("unknown LLM_REASONING={other} (expected on or off)"),
        };
        Ok(Some(LlmConfig {
            server_bin: PathBuf::from(var_or("LLAMA_SERVER", "/opt/llama.cpp/llama-server")),
            source,
            model_name: var("LLM_MODEL_NAME").unwrap_or(default_name),
            family,
            device: var_or("LLM_DEVICE", "none"),
            gpu_layers: var_or("LLM_GPU_LAYERS", "auto"),
            ctx,
            reasoning,
            reasoning_budget: var_or("LLM_REASONING_BUDGET", "512"),
            api_key: var("LLM_API_KEY"),
            extra_args: var("LLM_EXTRA_ARGS")
                .map(|a| a.split_whitespace().map(str::to_owned).collect())
                .unwrap_or_default(),
            slot_cache: on_off("LLM_SLOT_CACHE", true)?,
            template_fix: on_off("LLM_TEMPLATE_FIX", true)?,
            internal_port: 8081,
            hf_cache_dir: llm_dir.join("hf"),
            slot_dir: llm_dir.join("slots"),
            templates_dir: PathBuf::from(var_or("TEMPLATES_DIR", "/app/templates")),
        }))
    }

    fn search_from_env() -> Result<Option<SearchConfig>> {
        if !on_off("SEARCH_ENABLED", false)? {
            return Ok(None);
        }
        let paths = var_or("SEARXNG_PYTHONPATH", "/opt/searxng/src:/opt/searxng/lib");
        Ok(Some(SearchConfig {
            // SEARCH_LANGUAGE may be set to an empty value on purpose (all languages).
            language: env::var("SEARCH_LANGUAGE").unwrap_or_else(|_| "en".to_owned()),
            python_paths: env::split_paths(&paths).collect(),
            settings_path: PathBuf::from("/tmp/searxng-settings.yml"),
        }))
    }

    fn jellyfin_from_env() -> Result<Option<JellyfinConfig>> {
        let (Some(url), Some(api_key)) = (var("JELLYFIN_URL"), var("JELLYFIN_API_KEY")) else {
            return Ok(None);
        };
        let Some(user) = var("JELLYFIN_USER") else {
            bail!("set JELLYFIN_USER (the Jellyfin user whose next episodes count)");
        };
        Ok(Some(JellyfinConfig { url, api_key, user }))
    }

    fn media_from_env() -> Result<Option<crate::media::MediaConfig>> {
        if !on_off("MEDIA_INDEX", false)? {
            return Ok(None);
        }
        if var("JELLYFIN_URL").is_none() || var("JELLYFIN_API_KEY").is_none() {
            bail!("MEDIA_INDEX=on needs JELLYFIN_URL and JELLYFIN_API_KEY");
        }
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
        let gate = crate::media::Gate::default();
        let dims: usize = parse_num("EMBED_DIMS", 512)?;
        if !(64..=768).contains(&dims) {
            bail!("EMBED_DIMS={dims} must be between 64 and 768");
        }
        Ok(Some(crate::media::MediaConfig {
            lib_llama: PathBuf::from(var_or("LLAMA_LIB", "/opt/llama.cpp/libllama.so")),
            model: PathBuf::from(var_or("EMBED_MODEL", "/models/llm/embed/embeddinggemma-2-Q8_0.gguf")),
            model_url: var_or(
                "EMBED_MODEL_URL",
                "https://huggingface.co/ggml-org/embeddinggemma-2-GGUF/resolve/main/embeddinggemma-2-Q8_0.gguf",
            ),
            dims,
            threads: parse_num("EMBED_THREADS", i32::try_from(cores.min(4)).unwrap_or(4))?,
            dir: PathBuf::from(var_or("MEDIA_INDEX_DIR", "/models/llm/media-index")),
            episodes: on_off("MEDIA_INDEX_EPISODES", true)?,
            gate: crate::media::Gate {
                min_score: parse_num("MEDIA_MIN_SCORE", gate.min_score)?,
                min_margin: parse_num("MEDIA_MIN_MARGIN", gate.min_margin)?,
            },
        }))
    }

    fn youtube_from_env() -> Result<Option<YoutubeConfig>> {
        if !on_off("YOUTUBE_ENABLED", true)? {
            return Ok(None);
        }
        Ok(Some(YoutubeConfig {
            language: var_or("YOUTUBE_LANGUAGE", "en"),
            region: var_or("YOUTUBE_REGION", "DE"),
            client_version: var_or("YOUTUBE_CLIENT_VERSION", "2.20261030.00.00"),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_names() {
        assert_eq!(
            model_name_from_repo("inclusionAI/Ling-3.0-tiny-GGUF:Q4_K_M"),
            "Ling-3.0-tiny-Q4_K_M"
        );
        assert_eq!(model_name_from_repo("org/Model-GGUF"), "Model");
        assert_eq!(model_name_from_repo("Model"), "Model");
    }

    #[test]
    fn family_detection() {
        assert_eq!(
            Family::detect("inclusionAI/Ling-3.0-tiny-GGUF:Q4_K_M"),
            Some(Family::Ling3)
        );
        assert_eq!(Family::detect("MiniCPM5-2B-Q4_K_M.gguf"), Some(Family::Minicpm5));
        assert_eq!(Family::detect("LiquidAI/LFM3-1.7B-GGUF"), Some(Family::Lfm3));
        assert_eq!(Family::detect("Qwen3.5-4B.gguf"), Some(Family::Qwen35));
        assert_eq!(Family::detect("granite-4.0-h-tiny.gguf"), Some(Family::Granite4));
        assert_eq!(Family::detect("llama-3.gguf"), None);
    }

    #[test]
    fn slot_file_is_sanitized() {
        let cfg = LlmConfig {
            server_bin: PathBuf::new(),
            source: LlmSource::HfRepo("x".into()),
            model_name: "Ling 3/tiny:Q4".into(),
            family: Family::Ling3,
            device: "none".into(),
            gpu_layers: "auto".into(),
            ctx: Ctx::K64,
            reasoning: true,
            reasoning_budget: "512".into(),
            api_key: None,
            extra_args: vec![],
            slot_cache: true,
            template_fix: true,
            internal_port: 8081,
            hf_cache_dir: PathBuf::new(),
            slot_dir: PathBuf::new(),
            templates_dir: PathBuf::new(),
        };
        assert_eq!(cfg.slot_file(), "Ling_3_tiny_Q4-64k.slot");
    }
}
