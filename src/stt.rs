//! Speech to text with parakeet.cpp through its C API (`libparakeet.so`).
//!
//! The model loads once and stays in GPU memory. The library is opened at
//! runtime, so the binary builds without it.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use libloading::Library;

pub const RATE: u32 = 16_000;

/// Speech to text on 16 kHz mono samples in the range -1.0 to 1.0. Blocking.
pub trait SpeechToText: Send + Sync {
    fn transcribe(&self, samples: &[f32]) -> Result<String>;
}

type LoadFn = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type FreeFn = unsafe extern "C" fn(*mut c_void);
type TranscribeFn = unsafe extern "C" fn(*mut c_void, *const f32, c_int, c_int, c_int) -> *mut c_char;
type FreeStringFn = unsafe extern "C" fn(*mut c_char);
type LastErrorFn = unsafe extern "C" fn(*mut c_void) -> *const c_char;

struct Context_ {
    ctx: *mut c_void,
}

pub struct Parakeet {
    // Keeps the library loaded while the function pointers are in use.
    _lib: Library,
    ctx: Mutex<Context_>,
    free: FreeFn,
    transcribe: TranscribeFn,
    free_string: FreeStringFn,
    last_error: LastErrorFn,
}

// SAFETY: the parakeet context is only touched while the mutex is held, and
// the function pointers are plain C functions of a library that stays loaded.
unsafe impl Send for Parakeet {}
// SAFETY: see above, all access to the context goes through the mutex.
unsafe impl Sync for Parakeet {}

fn symbol<T: Copy>(lib: &Library, name: &str) -> Result<T> {
    // SAFETY: the type T matches the C declaration in parakeet's capi.h.
    unsafe {
        lib.get::<T>(name.as_bytes())
            .map(|s| *s)
            .with_context(|| format!("libparakeet: missing symbol {name}"))
    }
}

impl Parakeet {
    /// Loads the library and the model. `PARAKEET_DEVICE` must be set in the
    /// environment before (main does it before any thread starts).
    pub fn load(lib_path: &Path, model_path: &Path) -> Result<Self> {
        if !lib_path.is_file() {
            bail!("libparakeet not found: {}", lib_path.display());
        }
        if !model_path.is_file() {
            bail!("STT model not found: {}", model_path.display());
        }
        // SAFETY: loading a shared library runs its initializers. libparakeet
        // is a plain C/C++ library without unusual load-time behavior.
        let lib = unsafe { Library::new(lib_path) }.with_context(|| format!("cannot load {}", lib_path.display()))?;
        let load: LoadFn = symbol(&lib, "parakeet_capi_load")?;
        let free: FreeFn = symbol(&lib, "parakeet_capi_free")?;
        let transcribe: TranscribeFn = symbol(&lib, "parakeet_capi_transcribe_pcm")?;
        let free_string: FreeStringFn = symbol(&lib, "parakeet_capi_free_string")?;
        let last_error: LastErrorFn = symbol(&lib, "parakeet_capi_last_error")?;
        let path = CString::new(model_path.as_os_str().as_encoded_bytes())?;
        // SAFETY: path is a valid NUL-terminated string that outlives the call.
        let ctx = unsafe { load(path.as_ptr()) };
        if ctx.is_null() {
            bail!("parakeet_capi_load failed for {}", model_path.display());
        }
        tracing::info!("STT model loaded: {}", model_path.display());
        Ok(Self {
            _lib: lib,
            ctx: Mutex::new(Context_ { ctx }),
            free,
            transcribe,
            free_string,
            last_error,
        })
    }
}

impl SpeechToText for Parakeet {
    fn transcribe(&self, samples: &[f32]) -> Result<String> {
        if samples.is_empty() {
            return Ok(String::new());
        }
        let len = c_int::try_from(samples.len()).context("audio too long")?;
        let guard = self.ctx.lock().map_err(|_| anyhow::anyhow!("STT lock poisoned"))?;
        // SAFETY: ctx is a live context (we hold its mutex), the sample buffer
        // is valid for `len` floats during the call.
        let result = unsafe { (self.transcribe)(guard.ctx, samples.as_ptr(), len, RATE as c_int, 0) };
        if result.is_null() {
            // SAFETY: last_error returns NULL or a string owned by the context.
            let err = unsafe { (self.last_error)(guard.ctx) };
            let msg = if err.is_null() {
                String::from("unknown parakeet error")
            } else {
                // SAFETY: err is a valid NUL-terminated string (checked non-null).
                unsafe { CStr::from_ptr(err) }.to_string_lossy().into_owned()
            };
            bail!(msg);
        }
        // SAFETY: result is a valid NUL-terminated string from parakeet, freed below.
        let text = unsafe { CStr::from_ptr(result) }.to_string_lossy().trim().to_owned();
        // SAFETY: result came from parakeet_capi_transcribe_pcm and is freed once.
        unsafe { (self.free_string)(result) };
        Ok(text)
    }
}

impl Drop for Parakeet {
    fn drop(&mut self) {
        if let Ok(guard) = self.ctx.lock() {
            // SAFETY: the context is freed once, while the library is still loaded.
            unsafe { (self.free)(guard.ctx) };
        }
    }
}

/// How many recordings `STT_DEBUG_DIR` keeps.
const DEBUG_KEEP: usize = 200;

/// Saves one STT request as a 16 kHz mono WAV plus its transcript. Only the
/// newest 200 stay. The recordings contain everything that was said after the
/// wake word, so the option is off by default.
pub fn save_debug_recording(dir: &Path, pcm: &[i16], text: &str) {
    if let Err(err) = save_debug_inner(dir, pcm, text) {
        // A full or read-only disk must never break STT.
        tracing::warn!("STT debug recording failed: {err}");
    }
}

fn save_debug_inner(dir: &Path, pcm: &[i16], text: &str) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let stem = chrono::Local::now().format("%Y%m%d-%H%M%S-%3f").to_string();
    std::fs::write(dir.join(format!("{stem}.wav")), crate::audio::wav_bytes(pcm, RATE, 1))?;
    std::fs::write(dir.join(format!("{stem}.txt")), format!("{text}\n"))?;
    let mut wavs: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "wav"))
        .collect();
    wavs.sort();
    if wavs.len() > DEBUG_KEEP {
        for old in &wavs[..wavs.len() - DEBUG_KEEP] {
            let _ = std::fs::remove_file(old);
            let _ = std::fs::remove_file(old.with_extension("txt"));
        }
    }
    Ok(())
}
