//! Optional cleanup of the STT audio with Hush before Parakeet hears it.
//!
//! Hush (weya-ai/hush) is a DeepFilterNet3 model trained to remove noise and a
//! competing speaker, such as a TV in the room. It runs on the CPU through the
//! C API of `libweya_nc.so`, the upstream wrapper around DeepFilterNet's `df`
//! crate with `tract`. The image builds that library from a pinned Hush commit
//! and bakes the 16 kHz model bundle at a pinned revision (Dockerfile).
//!
//! Rules:
//!
//! 1. Off by default (`STT_DENOISE=off`). A model that changes what STT hears
//!    must prove itself on the user's rooms first.
//! 2. The model loads once and is shared. Every request gets its own session,
//!    so two satellites can speak at the same time without a lock.
//! 3. A failure never costs the transcript: STT then hears the raw audio.
//! 4. The output keeps the length and the timing of the input. DeepFilterNet
//!    delays its output by `fft_size - hop_size` samples, which is one frame
//!    for this model, so the frames run one frame past the end and the first
//!    frame of the output is dropped.

use std::ffi::{CString, c_char, c_float, c_void};
use std::path::Path;

use anyhow::{Context, Result, bail};
use libloading::Library;

use crate::stt::RATE;

/// Cleans 16 kHz mono samples in the range -1.0 to 1.0. Blocking.
pub trait Denoise: Send + Sync {
    fn clean(&self, samples: &[f32]) -> Result<Vec<f32>>;
}

type ModelLoadFn = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type ModelFreeFn = unsafe extern "C" fn(*mut c_void);
type SessionCreateFn = unsafe extern "C" fn(*const c_void, usize, c_float) -> *mut c_void;
type SessionFreeFn = unsafe extern "C" fn(*mut c_void);
type FrameLengthFn = unsafe extern "C" fn(*const c_void) -> usize;
type ProcessFrameFn = unsafe extern "C" fn(*mut c_void, *const c_float, *mut c_float) -> c_float;

pub struct Hush {
    // Keeps the library loaded while the function pointers are in use.
    _lib: Library,
    model: *mut c_void,
    atten_lim_db: f32,
    model_free: ModelFreeFn,
    session_create: SessionCreateFn,
    session_free: SessionFreeFn,
    frame_length: FrameLengthFn,
    process_frame: ProcessFrameFn,
}

// SAFETY: the model handle is immutable after the load. libweya_nc shares its
// compiled plan between sessions through an `Arc` and documents concurrent
// sessions as safe. All mutable state lives in a session, and a session never
// leaves the call that created it.
unsafe impl Send for Hush {}
// SAFETY: see above, `clean` only reads the model handle.
unsafe impl Sync for Hush {}

fn symbol<T: Copy>(lib: &Library, name: &str) -> Result<T> {
    // SAFETY: the type T matches the C declaration in Hush's weya_nc.h.
    unsafe {
        lib.get::<T>(name.as_bytes())
            .map(|s| *s)
            .with_context(|| format!("libweya_nc: missing symbol {name}"))
    }
}

impl Hush {
    /// Loads the library and the model bundle (`*_onnx.tar.gz`).
    pub fn load(lib_path: &Path, model_path: &Path, atten_lim_db: f32) -> Result<Self> {
        if !lib_path.is_file() {
            bail!("libweya_nc not found: {}", lib_path.display());
        }
        if !model_path.is_file() {
            bail!("Hush model not found: {}", model_path.display());
        }
        // SAFETY: loading a shared library runs its initializers. libweya_nc is
        // a Rust cdylib without load-time side effects.
        let lib = unsafe { Library::new(lib_path) }.with_context(|| format!("cannot load {}", lib_path.display()))?;
        let model_load: ModelLoadFn = symbol(&lib, "weya_nc_model_load_from_path")?;
        let model_free: ModelFreeFn = symbol(&lib, "weya_nc_model_free")?;
        let session_create: SessionCreateFn = symbol(&lib, "weya_nc_session_create")?;
        let session_free: SessionFreeFn = symbol(&lib, "weya_nc_session_free")?;
        let frame_length: FrameLengthFn = symbol(&lib, "weya_nc_get_frame_length")?;
        let process_frame: ProcessFrameFn = symbol(&lib, "weya_nc_process_frame")?;
        let path = CString::new(model_path.as_os_str().as_encoded_bytes())?;
        // SAFETY: path is a valid NUL-terminated string that outlives the call.
        let model = unsafe { model_load(path.as_ptr()) };
        if model.is_null() {
            bail!(
                "weya_nc_model_load_from_path failed for {} (see stderr)",
                model_path.display()
            );
        }
        let hush = Self {
            _lib: lib,
            model,
            atten_lim_db,
            model_free,
            session_create,
            session_free,
            frame_length,
            process_frame,
        };
        // One empty run checks the frame size before the first request needs it.
        hush.clean(&[0.0; 3200]).context("Hush test run failed")?;
        tracing::info!(
            "STT cleanup loaded: Hush {} (attenuation limit {} dB)",
            model_path.display(),
            atten_lim_db
        );
        Ok(hush)
    }
}

impl Denoise for Hush {
    fn clean(&self, samples: &[f32]) -> Result<Vec<f32>> {
        // SAFETY: the model handle is live until drop, the rate is the one the
        // bundle was trained for.
        let session = unsafe { (self.session_create)(self.model, RATE as usize, self.atten_lim_db) };
        if session.is_null() {
            bail!("weya_nc_session_create failed");
        }
        // SAFETY: session is a live session of this library.
        let frame = unsafe { (self.frame_length)(session) };
        let result = if frame == 0 {
            Err(anyhow::anyhow!("weya_nc reports a frame length of 0"))
        } else {
            Ok(run_frames(samples, frame, frame, |input, output| {
                // SAFETY: both slices hold exactly `frame` samples, the length
                // that the session asks for, and the session is live.
                unsafe { (self.process_frame)(session, input.as_ptr(), output.as_mut_ptr()) };
            }))
        };
        // SAFETY: the session is freed once, after its last use.
        unsafe { (self.session_free)(session) };
        result
    }
}

impl Drop for Hush {
    fn drop(&mut self) {
        // SAFETY: no session outlives `clean`, so the model has no users left.
        unsafe { (self.model_free)(self.model) };
    }
}

/// Runs `process` over `frame`-sized blocks and removes `delay` samples of
/// algorithmic delay: the input gets `delay` samples of silence at the end,
/// the first `delay` output samples are dropped, and the result has the
/// length of `samples`.
pub fn run_frames(
    samples: &[f32],
    frame: usize,
    delay: usize,
    mut process: impl FnMut(&[f32], &mut [f32]),
) -> Vec<f32> {
    let total = (samples.len() + delay).div_ceil(frame) * frame;
    let mut input = Vec::with_capacity(total);
    input.extend_from_slice(samples);
    input.resize(total, 0.0);
    let mut output = vec![0.0f32; total];
    for (inp, out) in input.chunks_exact(frame).zip(output.chunks_exact_mut(frame)) {
        process(inp, out);
    }
    output.drain(..delay.min(output.len()));
    output.truncate(samples.len());
    output
}

/// 16-bit PCM for the debug recordings, with clipping.
pub fn to_i16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|&s| (s * 32768.0).round().clamp(-32768.0, 32767.0) as i16)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A filter that delays by `delay` samples, like DeepFilterNet does.
    fn delaying(delay: usize) -> impl FnMut(&[f32], &mut [f32]) {
        let mut history: Vec<f32> = vec![0.0; delay];
        move |input, output| {
            history.extend_from_slice(input);
            output.copy_from_slice(&history[..input.len()]);
            history.drain(..input.len());
        }
    }

    #[test]
    fn removes_the_delay_and_keeps_the_length() {
        let samples: Vec<f32> = (0..1000).map(|i| i as f32).collect();
        let out = run_frames(&samples, 160, 160, delaying(160));
        assert_eq!(out, samples);
    }

    #[test]
    fn works_for_lengths_that_are_no_multiple_of_the_frame() {
        for len in [0, 1, 159, 160, 161, 479, 4801] {
            let samples: Vec<f32> = (0..len).map(|i| (i % 7) as f32).collect();
            let out = run_frames(&samples, 160, 160, delaying(160));
            assert_eq!(out, samples, "length {len}");
        }
    }

    #[test]
    fn calls_the_filter_with_full_frames_only() {
        let mut sizes = Vec::new();
        run_frames(&[0.5; 500], 160, 160, |i, o| {
            sizes.push(i.len());
            o.copy_from_slice(i);
        });
        assert!(sizes.iter().all(|&s| s == 160));
        // 500 samples plus 160 of delay need 5 frames.
        assert_eq!(sizes.len(), 5);
    }

    #[test]
    fn converts_to_pcm_with_clipping() {
        assert_eq!(
            to_i16(&[0.0, 0.5, -1.0, 1.5, -1.5]),
            vec![0, 16384, -32768, 32767, -32768]
        );
    }

    /// Runs the real library over a WAV file and writes the cleaned audio.
    /// `HUSH_LIB`, `HUSH_MODEL`, `HUSH_IN` and `HUSH_OUT` name the files.
    #[test]
    #[ignore = "needs libweya_nc.so, the Hush bundle and a 16 kHz WAV"]
    fn hush_cleans_a_recording() {
        let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("set {k}"));
        let hush = Hush::load(Path::new(&var("HUSH_LIB")), Path::new(&var("HUSH_MODEL")), 100.0).unwrap();
        let wav = std::fs::read(var("HUSH_IN")).unwrap();
        let (fmt, raw) = crate::audio::parse_wav(&wav).unwrap();
        let pcm = crate::audio::to_mono_i16(raw, fmt, RATE);
        let samples: Vec<f32> = pcm.iter().map(|&s| f32::from(s) / 32768.0).collect();
        let start = std::time::Instant::now();
        let clean = hush.clean(&samples).unwrap();
        let took = start.elapsed();
        assert_eq!(clean.len(), samples.len());
        println!(
            "{:.2} s of audio in {:.0} ms",
            samples.len() as f32 / RATE as f32,
            took.as_secs_f64() * 1000.0
        );
        std::fs::write(var("HUSH_OUT"), crate::audio::wav_bytes(&to_i16(&clean), RATE, 1)).unwrap();
    }
}
