//! Text embeddings with EmbeddingGemma 2 through llama.cpp's C API.
//!
//! `libllama.so` from the image (the same llama.cpp build as llama-server) is
//! opened at runtime, like `libparakeet.so`. There is no second server
//! process: the model (about 310 MB as Q8_0) lives in this process.
//!
//! Two rules keep it apart from the other engines:
//!
//! 1. Libraries open with `RTLD_LOCAL` (the libloading default).
//!    `libparakeet.so` carries its own copy of ggml with the same symbol
//!    names. Local loading keeps each library bound to its own ggml.
//! 2. Only a CPU backend of ggml is loaded, never the Vulkan backend. The
//!    embedder runs on the CPU, and the GPU stays with STT and TTS.
//!
//! The model maps text into a 768-dimensional space. Matryoshka training
//! allows a shorter prefix of each vector, so the index keeps the first
//! `dims` values and normalizes them again.

use std::ffi::{CString, c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use libloading::Library;

use super::ggml_sys::{self, Ggml};
use super::llama_sys::{self as sys, Llama};

/// Maps texts to L2-normalized vectors of length `dims()`. Blocking.
pub trait Embedder: Send + Sync {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    fn dims(&self) -> usize;
}

/// EmbeddingGemma 2 prompt for a search query.
pub fn query_text(query: &str) -> String {
    format!("task: search result | query: {query}")
}

/// EmbeddingGemma 2 prompt for a document.
pub fn document_text(title: &str, text: &str) -> String {
    format!("title: {title} | text: {text}")
}

/// The loaded model. Several contexts can use it at the same time.
pub struct Model {
    llama: Llama,
    // Keeps libggml loaded as long as the model.
    _ggml: Ggml,
    model: *mut sys::llama_model,
    n_embd: usize,
    encoder_only: bool,
}

// SAFETY: llama.cpp allows one model to be shared by contexts on several
// threads. The model is only read after loading, and freed once in Drop.
unsafe impl Send for Model {}
// SAFETY: see above.
unsafe impl Sync for Model {}

/// llama.cpp log lines go to the debug log, errors to warnings. Its warnings
/// at load time are about the model file ("control-looking token ... was not
/// control-type") and appear at every start, so they are debug output too.
unsafe extern "C" fn log_callback(level: sys::ggml_log_level, text: *const c_char, _: *mut c_void) {
    if text.is_null() {
        return;
    }
    // SAFETY: llama.cpp passes a valid NUL-terminated string.
    let line = unsafe { std::ffi::CStr::from_ptr(text) }.to_string_lossy();
    let line = line.trim_end();
    if line.is_empty() {
        return;
    }
    // ggml levels: 1 debug, 2 info, 3 warn, 4 error, 5 continuation of the
    // previous line (for example the progress dots of a model load).
    if level == 4 {
        tracing::warn!("[embed] {line}");
    } else {
        tracing::debug!("[embed] {line}");
    }
}

/// Loads the best CPU backend variant of ggml from `dir`, as ggml itself
/// does: each `libggml-cpu-*.so` reports a score for this CPU, 0 if it cannot
/// run here.
fn load_cpu_backend(ggml: &Ggml, dir: &Path) -> Result<()> {
    // SAFETY: plain C call without arguments.
    if unsafe { ggml.ggml_backend_dev_count() } > 0 {
        return Ok(());
    }
    let mut best: Option<(c_int, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))? {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !(name.starts_with("libggml-cpu") && name.ends_with(".so")) {
            continue;
        }
        // SAFETY: a ggml CPU backend library. Opening it runs no CPU-specific
        // code, and ggml_backend_score only reads CPU features.
        let score = unsafe {
            let lib = Library::new(&path)?;
            match lib.get::<unsafe extern "C" fn() -> c_int>(b"ggml_backend_score\0") {
                Ok(f) => f(),
                Err(_) => 0,
            }
        };
        if score > 0 && best.as_ref().is_none_or(|(s, _)| score > *s) {
            best = Some((score, path));
        }
    }
    let (_, path) = best.context("no ggml CPU backend works on this CPU")?;
    let cpath = CString::new(path.as_os_str().as_encoded_bytes())?;
    // SAFETY: cpath is a valid NUL-terminated path.
    let reg = unsafe { ggml.ggml_backend_load(cpath.as_ptr()) };
    if reg.is_null() {
        bail!("ggml could not load {}", path.display());
    }
    tracing::info!("embedder: ggml CPU backend {}", path.display());
    Ok(())
}

impl Model {
    /// Opens `libllama.so` (and `libggml.so` next to it) and loads the model on the CPU.
    pub fn load(lib_llama: &Path, model_path: &Path) -> Result<Arc<Self>> {
        let dir = lib_llama.parent().context("libllama path has no folder")?;
        // SAFETY: libggml and libllama are the C libraries of the pinned
        // llama.cpp build. The bindings were generated from its headers, and
        // their layouts are checked at compile time.
        let ggml = unsafe { Ggml::new(dir.join("libggml.so")) }
            .with_context(|| format!("cannot load {}", dir.join("libggml.so").display()))?;
        // SAFETY: see above.
        let llama = unsafe { Llama::new(lib_llama) }.with_context(|| format!("cannot load {}", lib_llama.display()))?;
        // Before the backend loads: llama_log_set also sets the ggml logger,
        // and ggml logs the load of the CPU backend.
        // SAFETY: plain C call. The callback is a static function.
        unsafe { llama.llama_log_set(Some(log_callback), std::ptr::null_mut()) };
        load_cpu_backend(&ggml, dir)?;

        // SAFETY: plain C call without arguments.
        unsafe { llama.llama_backend_init() };

        // The CPU device only, as a NULL-terminated list.
        let mut cpu: Option<ggml_sys::ggml_backend_dev_t> = None;
        // SAFETY: indices below dev_count are valid devices.
        unsafe {
            for i in 0..ggml.ggml_backend_dev_count() {
                let dev = ggml.ggml_backend_dev_get(i);
                if ggml.ggml_backend_dev_type(dev) == ggml_sys::ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_CPU {
                    cpu = Some(dev);
                    break;
                }
            }
        }
        let cpu = cpu.context("ggml has no CPU device")?;
        let mut devices: [sys::ggml_backend_dev_t; 2] = [cpu.cast(), std::ptr::null_mut()];

        // SAFETY: the default params are plain data. devices outlives the call.
        let model = unsafe {
            let mut params = llama.llama_model_default_params();
            params.devices = devices.as_mut_ptr();
            params.n_gpu_layers = 0;
            let path = CString::new(model_path.as_os_str().as_encoded_bytes())?;
            llama.llama_model_load_from_file(path.as_ptr(), params)
        };
        if model.is_null() {
            bail!("llama.cpp could not load {}", model_path.display());
        }
        // SAFETY: model is a valid, loaded model.
        let (n_embd, encoder_only) = unsafe {
            (
                usize::try_from(llama.llama_model_n_embd_out(model)).unwrap_or(0),
                llama.llama_model_has_encoder(model) && !llama.llama_model_has_decoder(model),
            )
        };
        tracing::info!("embedder: {} loaded, {n_embd} dimensions", model_path.display());
        Ok(Arc::new(Self {
            llama,
            _ggml: ggml,
            model,
            n_embd,
            encoder_only,
        }))
    }
}

impl Drop for Model {
    fn drop(&mut self) {
        // SAFETY: freed once. Every context holds an Arc, so none is left.
        unsafe { self.llama.llama_model_free(self.model) };
    }
}

struct Ctx(*mut sys::llama_context);

/// One llama.cpp context on the shared model. Calls are serialized by a mutex.
pub struct EmbedContext {
    model: Arc<Model>,
    ctx: Mutex<Ctx>,
    n_ctx: usize,
    dims: usize,
}

// SAFETY: the raw context is only used while its mutex is held.
unsafe impl Send for EmbedContext {}
// SAFETY: see above.
unsafe impl Sync for EmbedContext {}

impl EmbedContext {
    /// A context for texts of up to `max_tokens` tokens (longer ones are cut).
    pub fn new(model: Arc<Model>, max_tokens: u32, threads: i32, dims: usize) -> Result<Self> {
        if dims == 0 || dims > model.n_embd {
            bail!("embedding size {dims} is not between 1 and {}", model.n_embd);
        }
        let llama = &model.llama;
        // SAFETY: plain data in, a context or NULL out.
        let ctx = unsafe {
            let mut p = llama.llama_context_default_params();
            p.n_ctx = max_tokens;
            p.n_batch = max_tokens;
            p.n_ubatch = max_tokens;
            p.n_seq_max = 1;
            p.n_threads = threads;
            p.n_threads_batch = threads;
            p.embeddings = true;
            p.pooling_type = sys::llama_pooling_type_LLAMA_POOLING_TYPE_UNSPECIFIED;
            p.no_perf = true;
            llama.llama_init_from_model(model.model, p)
        };
        if ctx.is_null() {
            bail!("llama.cpp could not create an embedding context");
        }
        // SAFETY: ctx is valid.
        let n_ctx = unsafe { llama.llama_n_ctx(ctx) } as usize;
        Ok(Self {
            model,
            ctx: Mutex::new(Ctx(ctx)),
            n_ctx,
            dims,
        })
    }

    fn tokenize(&self, text: &str) -> Result<Vec<sys::llama_token>> {
        let llama = &self.model.llama;
        // SAFETY: the model is valid. The text pointer and length match.
        let vocab = unsafe { llama.llama_model_get_vocab(self.model.model) };
        let len = i32::try_from(text.len()).context("text too long")?;
        let mut tokens = vec![0; text.len() + 8];
        // SAFETY: tokens has room for `tokens.len()` entries.
        let mut n = unsafe {
            llama.llama_tokenize(
                vocab,
                text.as_ptr().cast(),
                len,
                tokens.as_mut_ptr(),
                tokens.len() as i32,
                true,
                false,
            )
        };
        if n < 0 {
            tokens.resize(n.unsigned_abs() as usize, 0);
            // SAFETY: as above, with the size that llama.cpp asked for.
            n = unsafe {
                llama.llama_tokenize(
                    vocab,
                    text.as_ptr().cast(),
                    len,
                    tokens.as_mut_ptr(),
                    tokens.len() as i32,
                    true,
                    false,
                )
            };
            if n < 0 {
                bail!("tokenize failed");
            }
        }
        tokens.truncate(n as usize);
        tokens.truncate(self.n_ctx);
        Ok(tokens)
    }

    fn embed_one(&self, ctx: &Ctx, text: &str) -> Result<Vec<f32>> {
        let llama = &self.model.llama;
        let tokens = self.tokenize(text)?;
        if tokens.is_empty() {
            bail!("empty text");
        }
        let n = tokens.len();
        // SAFETY: the batch has room for n tokens of one sequence. Every
        // pointer that is written below belongs to it, and it is freed once.
        unsafe {
            let mut batch = llama.llama_batch_init(n as i32, 0, 1);
            for (i, &tok) in tokens.iter().enumerate() {
                *batch.token.add(i) = tok;
                *batch.pos.add(i) = i as i32;
                *batch.n_seq_id.add(i) = 1;
                **batch.seq_id.add(i) = 0;
                *batch.logits.add(i) = 1;
            }
            batch.n_tokens = n as i32;
            llama.llama_memory_clear(llama.llama_get_memory(ctx.0), true);
            let rc = if self.model.encoder_only {
                llama.llama_encode(ctx.0, batch)
            } else {
                llama.llama_decode(ctx.0, batch)
            };
            llama.llama_batch_free(batch);
            if rc != 0 {
                bail!("llama.cpp embedding failed ({rc})");
            }
            let out = llama.llama_get_embeddings_seq(ctx.0, 0);
            if out.is_null() {
                bail!("llama.cpp returned no pooled embedding (pooling type of the model?)");
            }
            let full = std::slice::from_raw_parts(out, self.model.n_embd);
            Ok(normalize(&full[..self.dims]))
        }
    }
}

impl Embedder for EmbedContext {
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let ctx = self.ctx.lock().map_err(|_| anyhow::anyhow!("embedder lock poisoned"))?;
        texts.iter().map(|t| self.embed_one(&ctx, t)).collect()
    }

    fn dims(&self) -> usize {
        self.dims
    }
}

impl Drop for EmbedContext {
    fn drop(&mut self) {
        if let Ok(ctx) = self.ctx.lock() {
            // SAFETY: freed once, before the model (the Arc is still held).
            unsafe { self.model.llama.llama_free(ctx.0) };
        }
    }
}

/// Scales a vector to length 1.
pub fn normalize(v: &[f32]) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm == 0.0 {
        return v.to_vec();
    }
    v.iter().map(|x| x / norm).collect()
}

/// Dot product of two normalized vectors (cosine similarity).
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompts_and_math() {
        assert_eq!(query_text("spy family"), "task: search result | query: spy family");
        assert_eq!(document_text("Dune", "Movie"), "title: Dune | text: Movie");
        let v = normalize(&[3.0, 4.0]);
        assert!((dot(&v, &v) - 1.0).abs() < 1e-6);
        assert_eq!(normalize(&[0.0, 0.0]), vec![0.0, 0.0]);
    }

    /// Needs llama.cpp and the model: `EMBED_TEST_LIB=/path/libllama.so
    /// EMBED_TEST_MODEL=/path/embeddinggemma-2-Q8_0.gguf cargo test real_model -- --ignored --nocapture`
    #[test]
    #[ignore = "needs libllama.so and the EmbeddingGemma 2 model"]
    fn real_model() {
        let lib = PathBuf::from(std::env::var("EMBED_TEST_LIB").unwrap());
        let model = Model::load(&lib, Path::new(&std::env::var("EMBED_TEST_MODEL").unwrap())).unwrap();
        let ctx = EmbedContext::new(model, 512, 4, 768).unwrap();
        let docs = [
            document_text(
                "Spy x Family",
                "Series. Anime, Comedy. A spy, an assassin and a telepath form a fake family.",
            ),
            document_text(
                "Dune",
                "Movie. Science Fiction. Paul Atreides on the desert planet Arrakis with giant sandworms.",
            ),
        ];
        let start = std::time::Instant::now();
        let d = ctx.embed(&docs).unwrap();
        println!("2 docs in {:?}", start.elapsed());
        let q = ctx.embed(&[query_text("desert planet movie with sand worms")]).unwrap();
        let s: Vec<f32> = d.iter().map(|v| dot(v, &q[0])).collect();
        println!("scores {s:?}");
        assert!(s[1] > s[0]);
        if let Ok(expect) = std::env::var("EMBED_TEST_REFERENCE") {
            // First values of llama-server's embedding of query_text("hello").
            let r = ctx.embed(&[query_text("hello")]).unwrap();
            println!("hello[0..4] = {:?}, reference {expect}", &r[0][..4]);
        }
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// `EMBED_TEST_LIB=... EMBED_TEST_MODEL=... cargo test --release bench_embed -- --ignored --nocapture`
    #[test]
    #[ignore = "needs libllama.so and the EmbeddingGemma 2 model"]
    fn bench_embed() {
        let lib = PathBuf::from(std::env::var("EMBED_TEST_LIB").unwrap());
        let model = Model::load(&lib, Path::new(&std::env::var("EMBED_TEST_MODEL").unwrap())).unwrap();
        let text = document_text(
            "Some Movie",
            &"A long plot summary with many words about people and places and events. ".repeat(10),
        );
        for threads in [1, 2, 4, 8] {
            let ctx = EmbedContext::new(model.clone(), 512, threads, 512).unwrap();
            let docs = vec![text.clone(); 20];
            let t = std::time::Instant::now();
            ctx.embed(&docs).unwrap();
            let per = t.elapsed().as_secs_f64() / 20.0;
            println!(
                "threads {threads}: {:.0} ms per document ({} tokens)",
                per * 1000.0,
                ctx.tokenize(&text).unwrap().len()
            );
        }
    }
}
