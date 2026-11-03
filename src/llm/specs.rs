//! The spec of the LLM child process for each start.
//!
//! At every start, [`LlmSpecs`] looks at the facts again (the device, the model
//! file, the CPU, the outcome of the last run) and picks the engine. So the
//! first start of a Hugging Face model runs llama.cpp for the download, and
//! the next start runs ik_llama.cpp. An ik_llama.cpp that fails twice is
//! replaced by llama.cpp (`engine.rs`).

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use crate::config::LlmConfig;
use crate::supervisor::{ChildSpec, Ended, RunOutcome, SpecSource};

use super::engine::{self, ActiveEngine, Chooser, Decision, EngineKind, Facts};
use super::{ik_spec, llamacpp_spec, model_file, slots_usable};

/// What the choice reads from the machine.
#[derive(Debug, Clone)]
pub struct Host {
    /// The text of /proc/cpuinfo.
    pub cpuinfo: String,
    /// The CPUs that the process may run on (its CPU set). A CPU quota of
    /// the container (`--cpus`) does not lower it: a quota lets more threads
    /// run for a shorter time, which was faster in a test.
    pub logical: usize,
}

/// The CPUs in the CPU set of this process.
fn allowed_cpus() -> usize {
    // SAFETY: `set` is a plain bit mask that sched_getaffinity fills in. The
    // call gets its true size, and the all-zero value is a valid empty set.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) == 0 {
            let n = libc::CPU_COUNT(&set);
            if n > 0 {
                return usize::try_from(n).unwrap_or(1);
            }
        }
    }
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

impl Host {
    pub fn detect() -> Self {
        Self {
            cpuinfo: std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default(),
            logical: allowed_cpus(),
        }
    }
}

pub struct LlmSpecs {
    cfg: LlmConfig,
    host: Host,
    chooser: Chooser,
    active: ActiveEngine,
    slots: bool,
    starts: Arc<AtomicU64>,
    last: Option<EngineKind>,
    /// A download-first run ended at its first good answer.
    download_done: bool,
}

/// What `main` needs besides the spec source.
pub struct Setup {
    pub specs: LlmSpecs,
    /// The prompt cache folder works.
    pub slots_usable: bool,
    /// The engine of the current start.
    pub active: ActiveEngine,
    /// Counts the starts of the child process.
    pub starts: Arc<AtomicU64>,
}

pub fn setup(cfg: &LlmConfig) -> Setup {
    setup_on(cfg, Host::detect())
}

pub fn setup_on(cfg: &LlmConfig, host: Host) -> Setup {
    let slots = slots_usable(cfg);
    let active = ActiveEngine::new(EngineKind::LlamaCpp);
    let starts = Arc::new(AtomicU64::new(0));
    Setup {
        specs: LlmSpecs {
            cfg: cfg.clone(),
            host,
            chooser: Chooser::default(),
            active: active.clone(),
            slots,
            starts: starts.clone(),
            last: None,
            download_done: false,
        },
        slots_usable: slots,
        active,
        starts,
    }
}

impl LlmSpecs {
    /// Chooses the engine and builds its spec. `last` is the outcome of the
    /// run before.
    pub async fn plan(&mut self, last: Option<RunOutcome>) -> (Decision, ChildSpec) {
        if let Some(kind) = self.last {
            self.chooser.note(kind, last);
        }
        if last.is_some_and(|o| o.ended == Ended::Replaced) {
            self.download_done = true;
        }
        let model = model_file(&self.cfg);
        let architecture = model
            .as_deref()
            .and_then(|m| crate::gguf::metadata_string(m, "general.architecture").ok().flatten());
        let decision = engine::decide(&Facts {
            choice: self.cfg.engine,
            device: &self.cfg.device,
            family: self.cfg.family,
            architecture: architecture.as_deref(),
            model_on_disk: model.is_some(),
            download_done: self.download_done,
            cpu_avx2_fma: engine::has_avx2_fma(&self.host.cpuinfo),
            ik_binary: self.cfg.ik_bin.is_file(),
            ik_banned: self.chooser.banned(),
        });
        tracing::info!("LLM engine: {} ({})", decision.kind.label(), decision.reason);
        self.last = Some(decision.kind);
        self.active.set(decision.kind);
        let spec = match (decision.kind, model) {
            (EngineKind::Ik, Some(model)) => ik_spec(
                &self.cfg,
                &model,
                self.slots,
                engine::ik_threads(&self.host.cpuinfo, self.host.logical),
                self.starts.clone(),
            ),
            _ => llamacpp_spec(&self.cfg, self.slots, self.starts.clone(), decision.download_first).await,
        };
        (decision, spec)
    }
}

impl SpecSource for LlmSpecs {
    async fn next(&mut self, last: Option<RunOutcome>) -> ChildSpec {
        self.plan(last).await.1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EngineChoice, Family, LlmSource};
    use std::path::Path;
    use std::time::Duration;

    const CPU: &str = "processor : 0\nphysical id : 0\ncore id : 0\nflags : sse2 avx2 fma\n\n\
                       processor : 1\nphysical id : 0\ncore id : 1\nflags : sse2 avx2 fma\n";

    fn host() -> Host {
        Host {
            cpuinfo: CPU.into(),
            logical: 2,
        }
    }

    struct Rig {
        dir: tempfile::TempDir,
        cfg: LlmConfig,
    }

    /// A Ling model file, a fake ik binary and a fake llama-server.
    fn rig(arch: &str) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("Ling-3.0-tiny-Q4_K_M.gguf");
        crate::gguf::write_test_gguf(&model, &[("general.architecture", arch)]);
        let ik = dir.path().join("ik-server");
        std::fs::write(&ik, b"#!/bin/sh\n").unwrap();
        let cfg = LlmConfig {
            source: LlmSource::File(model),
            ik_bin: ik,
            server_bin: dir.path().join("llama-server"),
            hf_cache_dir: dir.path().join("hf"),
            slot_dir: dir.path().join("slots"),
            template_fix: false,
            ..LlmConfig::for_test()
        };
        Rig { dir, cfg }
    }

    async fn plan(r: &Rig) -> (Decision, ChildSpec) {
        setup_on(&r.cfg, host()).specs.plan(None).await
    }

    #[tokio::test]
    async fn a_verified_model_on_the_cpu_starts_on_ik() {
        let r = rig("bailingmoe3");
        let (d, spec) = plan(&r).await;
        assert_eq!(d.kind, EngineKind::Ik);
        assert_eq!(spec.program, r.cfg.ik_bin);
        let a = spec.args.join(" ");
        assert!(a.starts_with("-m "), "{a}");
        assert!(a.contains("--webui none --log-disable -t 2 "), "2 physical cores: {a}");
        assert!(!spec.restart_when_healthy);
        assert_eq!(spec.health.unwrap().url, "http://127.0.0.1:8081/health");
    }

    #[tokio::test]
    async fn the_same_model_on_a_gpu_or_with_the_override_starts_on_llamacpp() {
        let mut r = rig("bailingmoe3");
        r.cfg.device = "Vulkan0".into();
        let (d, spec) = plan(&r).await;
        assert_eq!(
            (d.kind, spec.program.clone()),
            (EngineKind::LlamaCpp, r.cfg.server_bin.clone())
        );
        assert!(spec.args.join(" ").contains("--device Vulkan0"));

        let mut r = rig("bailingmoe3");
        r.cfg.engine = EngineChoice::LlamaCpp;
        assert_eq!(plan(&r).await.0.kind, EngineKind::LlamaCpp);
    }

    #[tokio::test]
    async fn an_unknown_architecture_or_a_missing_ik_binary_starts_on_llamacpp() {
        let r = rig("somethingnew");
        assert_eq!(plan(&r).await.0.kind, EngineKind::LlamaCpp);
        let mut r = rig("bailingmoe3");
        r.cfg.ik_bin = r.dir.path().join("missing");
        assert_eq!(plan(&r).await.0.kind, EngineKind::LlamaCpp);
    }

    #[tokio::test]
    async fn the_cpu_without_avx2_starts_on_llamacpp() {
        let r = rig("bailingmoe3");
        let mut s = setup_on(
            &r.cfg,
            Host {
                cpuinfo: "flags : sse2 avx".into(),
                logical: 4,
            },
        );
        assert_eq!(s.specs.plan(None).await.0.kind, EngineKind::LlamaCpp);
    }

    fn failed(ended: Ended) -> Option<RunOutcome> {
        Some(RunOutcome {
            ended,
            healthy: false,
            ran: Duration::from_secs(2),
        })
    }

    #[tokio::test]
    async fn two_failed_starts_of_ik_end_in_llamacpp() {
        let r = rig("bailingmoe3");
        let mut s = setup_on(&r.cfg, host());
        let active = s.active.clone();
        assert_eq!(s.specs.plan(None).await.0.kind, EngineKind::Ik);
        assert_eq!(active.get(), EngineKind::Ik, "the prompt cache learns the engine");
        assert_eq!(s.specs.plan(failed(Ended::Exited)).await.0.kind, EngineKind::Ik);
        let (d, spec) = s.specs.plan(failed(Ended::Exited)).await;
        assert_eq!(d.kind, EngineKind::LlamaCpp);
        assert_eq!(active.get(), EngineKind::LlamaCpp);
        assert_eq!(spec.program, r.cfg.server_bin);
        // It stays on llama.cpp.
        assert_eq!(s.specs.plan(failed(Ended::Exited)).await.0.kind, EngineKind::LlamaCpp);
    }

    #[tokio::test]
    async fn the_first_start_downloads_with_llamacpp_and_the_next_one_runs_ik() {
        let mut r = rig("bailingmoe3");
        r.cfg.source = LlmSource::HfRepo("inclusionAI/Ling-3.0-tiny-GGUF:Q4_K_M".into());
        let mut s = setup_on(&r.cfg, host());
        let (d, spec) = s.specs.plan(None).await;
        assert!(d.download_first && d.kind == EngineKind::LlamaCpp);
        assert!(spec.restart_when_healthy, "llama.cpp is replaced after the download");
        assert!(spec.args.contains(&"-hf".to_owned()));
        // llama.cpp has downloaded the file into its cache.
        let snap = r
            .cfg
            .hf_cache_dir
            .join("models--inclusionAI--Ling-3.0-tiny-GGUF/snapshots/abc");
        std::fs::create_dir_all(&snap).unwrap();
        crate::gguf::write_test_gguf(
            &snap.join("Ling-3.0-tiny-Q4_K_M.gguf"),
            &[("general.architecture", "bailingmoe3")],
        );
        let replaced = Some(RunOutcome {
            ended: Ended::Replaced,
            healthy: true,
            ran: Duration::from_secs(60),
        });
        let (d, spec) = s.specs.plan(replaced).await;
        assert_eq!(d.kind, EngineKind::Ik);
        assert!(spec.args.iter().any(|a| a.contains("Ling-3.0-tiny-Q4_K_M.gguf")));
        assert!(!spec.args.contains(&"-hf".to_owned()));
    }

    #[tokio::test]
    async fn a_download_that_the_hub_does_not_find_runs_on_llamacpp_without_more_restarts() {
        let mut r = rig("bailingmoe3");
        r.cfg.source = LlmSource::HfRepo("inclusionAI/Ling-3.0-tiny-GGUF".into());
        let mut s = setup_on(&r.cfg, host());
        assert!(s.specs.plan(None).await.0.download_first);
        // llama.cpp chose another quantization than the q4_k_m that the hub
        // looks for, so the cache has no matching file.
        let replaced = Some(RunOutcome {
            ended: Ended::Replaced,
            healthy: true,
            ran: Duration::from_secs(60),
        });
        let (d, spec) = s.specs.plan(replaced).await;
        assert_eq!(d.kind, EngineKind::LlamaCpp);
        assert!(!d.download_first);
        assert!(!spec.restart_when_healthy);
    }

    #[tokio::test]
    async fn families_that_ik_is_not_verified_for_stay_on_llamacpp() {
        let mut r = rig("qwen35");
        r.cfg.family = Family::Qwen35;
        assert_eq!(plan(&r).await.0.kind, EngineKind::LlamaCpp);
        // The override forces ik for any family.
        r.cfg.engine = EngineChoice::Ik;
        assert_eq!(plan(&r).await.0.kind, EngineKind::Ik);
    }

    #[tokio::test]
    async fn the_spec_source_counts_starts_in_one_counter() {
        let r = rig("bailingmoe3");
        let mut s = setup_on(&r.cfg, host());
        let a = s.specs.next(None).await;
        let b = s.specs.next(None).await;
        assert!(Arc::ptr_eq(&a.starts, &b.starts));
        assert!(Arc::ptr_eq(&a.starts, &s.starts));
        let _ = Path::new("");
    }

    #[test]
    fn the_cpu_set_of_this_process_is_found() {
        let n = allowed_cpus();
        let all = std::thread::available_parallelism().map_or(1, |n| n.get());
        assert!(n >= 1 && n <= all.max(n), "{n} CPUs");
        // The test process runs on at least one CPU, and no more than the machine has.
        assert!(n <= 4096);
    }
}
