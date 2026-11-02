//! Which program runs the model: llama.cpp or ik_llama.cpp.
//!
//! Both are `llama-server` programs with an OpenAI compatible API. The server
//! hides which one runs. The rest of the code sees one URL, one health check
//! and one prompt cache.
//!
//! ik_llama.cpp runs the CPU only and reads the prompt 1.6 to 2.3 times faster
//! (docs/llm.md). So the server uses it when the model runs on the CPU, the
//! model is one that the evaluation verified, and the CPU has AVX2 and FMA.
//! In every other case llama.cpp runs. An ik_llama.cpp that fails to start
//! twice in a row is replaced by llama.cpp until the server restarts.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use crate::config::{EngineChoice, Family};
use crate::supervisor::{Ended, RunOutcome};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineKind {
    LlamaCpp,
    Ik,
}

impl EngineKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::LlamaCpp => "llama.cpp",
            Self::Ik => "ik_llama.cpp",
        }
    }
}

/// The engine of the current start. The prompt cache reads it to choose its
/// file.
#[derive(Debug, Clone)]
pub struct ActiveEngine(Arc<AtomicU8>);

impl ActiveEngine {
    pub fn new(kind: EngineKind) -> Self {
        let a = Self(Arc::new(AtomicU8::new(0)));
        a.set(kind);
        a
    }

    pub fn get(&self) -> EngineKind {
        match self.0.load(Ordering::SeqCst) {
            1 => EngineKind::Ik,
            _ => EngineKind::LlamaCpp,
        }
    }

    pub fn set(&self, kind: EngineKind) {
        self.0.store(u8::from(kind == EngineKind::Ik), Ordering::SeqCst);
    }
}

/// The `general.architecture` values of the model files that ik_llama.cpp
/// runs as well as llama.cpp, per family. The evaluation of docs/llm.md found
/// them. A family or an architecture that is not here runs on llama.cpp.
pub fn ik_architectures(family: Family) -> &'static [&'static str] {
    match family {
        Family::Ling3 => &["bailingmoe3"],
        Family::Minicpm5 => &["llama"],
        Family::Lfm3 => &["lfm3"],
        Family::Qwen35 | Family::Granite4 => &[],
    }
}

/// What the choice depends on.
#[derive(Debug, Clone)]
pub struct Facts<'a> {
    pub choice: EngineChoice,
    pub device: &'a str,
    pub family: Family,
    /// `general.architecture` of the model file. None: there is no readable
    /// file yet.
    pub architecture: Option<&'a str>,
    pub model_on_disk: bool,
    pub cpu_avx2_fma: bool,
    pub ik_binary: bool,
    /// Two starts of ik_llama.cpp in a row failed.
    pub ik_banned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub kind: EngineKind,
    /// A sentence for the log.
    pub reason: String,
    /// llama.cpp runs only to download the model. The server restarts it with
    /// the faster engine as soon as the model answers.
    pub download_first: bool,
}

fn llamacpp(reason: &str) -> Decision {
    Decision {
        kind: EngineKind::LlamaCpp,
        reason: reason.to_owned(),
        download_first: false,
    }
}

/// Chooses the engine for one start.
pub fn decide(f: &Facts) -> Decision {
    if f.choice == EngineChoice::LlamaCpp {
        return llamacpp("LLM_ENGINE=llamacpp");
    }
    if f.device != "none" {
        return llamacpp("the model runs on a GPU, and ik_llama.cpp runs on the CPU only");
    }
    if !f.ik_binary {
        return llamacpp("this image has no ik_llama.cpp");
    }
    if !f.cpu_avx2_fma {
        return llamacpp("the CPU lacks AVX2 or FMA, which ik_llama.cpp needs");
    }
    if f.ik_banned {
        return llamacpp("ik_llama.cpp failed to start twice");
    }
    let wanted = Decision {
        kind: EngineKind::Ik,
        reason: if f.choice == EngineChoice::Ik {
            "LLM_ENGINE=ik".to_owned()
        } else {
            "the CPU runs the model and ik_llama.cpp reads the prompt faster".to_owned()
        },
        download_first: false,
    };
    let verified = ik_architectures(f.family);
    if f.choice == EngineChoice::Auto && verified.is_empty() {
        return llamacpp("ik_llama.cpp is not verified for this model family");
    }
    if !f.model_on_disk {
        // ik_llama.cpp cannot download a model by repository name. llama.cpp
        // downloads it, then the server restarts on ik_llama.cpp.
        return Decision {
            kind: EngineKind::LlamaCpp,
            reason: format!("{}; llama.cpp downloads the model first", wanted.reason),
            download_first: true,
        };
    }
    if f.choice == EngineChoice::Auto {
        match f.architecture {
            Some(a) if verified.contains(&a) => {}
            Some(a) => {
                return llamacpp(&format!("ik_llama.cpp is not verified for the architecture {a}"));
            }
            None => return llamacpp("the architecture of the model file is unknown"),
        }
    }
    wanted
}

/// The time an ik_llama.cpp run must last to count as a good start.
const GOOD_RUN: Duration = Duration::from_secs(120);
const MAX_FAILURES: u32 = 2;

/// Remembers failed starts of ik_llama.cpp.
#[derive(Debug, Default)]
pub struct Chooser {
    failures: u32,
    banned: bool,
}

impl Chooser {
    pub fn banned(&self) -> bool {
        self.banned
    }

    /// Records how the last run of `kind` ended. A run that never answered, or
    /// that ended in less than two minutes, is a failure. A long run is a
    /// success.
    pub fn note(&mut self, kind: EngineKind, outcome: Option<RunOutcome>) {
        let Some(o) = outcome else { return };
        if kind != EngineKind::Ik || o.ended == Ended::Replaced {
            return;
        }
        if o.healthy && o.ran >= GOOD_RUN {
            self.failures = 0;
            return;
        }
        self.failures += 1;
        if self.failures >= MAX_FAILURES && !self.banned {
            self.banned = true;
            tracing::warn!(
                "ik_llama.cpp failed {} starts in a row, llama.cpp runs the model from now on",
                self.failures
            );
        }
    }
}

/// True if the `flags` line of /proc/cpuinfo has avx2 and fma.
pub fn has_avx2_fma(cpuinfo: &str) -> bool {
    cpuinfo.lines().find(|l| l.starts_with("flags")).is_some_and(|l| {
        let has = |f: &str| l.split_whitespace().any(|w| w == f);
        has("avx2") && has("fma")
    })
}

/// The number of physical cores: distinct (package, core) pairs of
/// /proc/cpuinfo. None if the file does not list them.
pub fn physical_cores(cpuinfo: &str) -> Option<usize> {
    let mut seen = std::collections::BTreeSet::new();
    let (mut package, mut core) = (None, None);
    for line in cpuinfo.lines().chain(std::iter::once("")) {
        if line.trim().is_empty() {
            if let (Some(p), Some(c)) = (package.take(), core.take()) {
                seen.insert((p, c));
            }
            package = None;
            core = None;
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "physical id" => package = Some(value.trim().to_owned()),
            "core id" => core = Some(value.trim().to_owned()),
            _ => {}
        }
    }
    (!seen.is_empty()).then_some(seen.len())
}

/// Threads for ik_llama.cpp, whose own default is a fixed 8: the physical
/// cores, at most 8. More threads than that do not make the reply faster, and
/// the efficiency cores of a hybrid CPU slow the others down. `logical` is
/// the CPU set of the process, so it caps the result: more threads than CPUs
/// are much slower.
pub fn ik_threads(cpuinfo: &str, logical: usize) -> usize {
    physical_cores(cpuinfo).map_or(logical, |p| p.min(logical)).clamp(1, 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Facts<'static> {
        Facts {
            choice: EngineChoice::Auto,
            device: "none",
            family: Family::Ling3,
            architecture: Some("bailingmoe3"),
            model_on_disk: true,
            cpu_avx2_fma: true,
            ik_binary: true,
            ik_banned: false,
        }
    }

    fn kind(f: &Facts) -> EngineKind {
        decide(f).kind
    }

    #[test]
    fn the_cpu_runs_a_verified_model_on_ik() {
        let d = decide(&facts());
        assert_eq!(d.kind, EngineKind::Ik);
        assert!(!d.download_first);
        for (family, arch) in [
            (Family::Ling3, "bailingmoe3"),
            (Family::Minicpm5, "llama"),
            (Family::Lfm3, "lfm3"),
        ] {
            let f = Facts {
                family,
                architecture: Some(arch),
                ..facts()
            };
            assert_eq!(kind(&f), EngineKind::Ik, "{family:?}");
        }
    }

    #[test]
    fn every_other_case_runs_on_llamacpp() {
        let on = |f: Facts| decide(&f);
        assert_eq!(
            on(Facts {
                choice: EngineChoice::LlamaCpp,
                ..facts()
            })
            .reason,
            "LLM_ENGINE=llamacpp"
        );
        // A GPU: ik_llama.cpp has no Vulkan.
        for device in ["Vulkan0", "Vulkan1"] {
            assert_eq!(on(Facts { device, ..facts() }).kind, EngineKind::LlamaCpp);
        }
        assert_eq!(
            on(Facts {
                ik_binary: false,
                ..facts()
            })
            .kind,
            EngineKind::LlamaCpp
        );
        assert_eq!(
            on(Facts {
                cpu_avx2_fma: false,
                ..facts()
            })
            .kind,
            EngineKind::LlamaCpp
        );
        assert_eq!(
            on(Facts {
                ik_banned: true,
                ..facts()
            })
            .kind,
            EngineKind::LlamaCpp
        );
        // Families without verification.
        for family in [Family::Qwen35, Family::Granite4] {
            let d = on(Facts {
                family,
                architecture: Some("qwen35"),
                ..facts()
            });
            assert_eq!(d.kind, EngineKind::LlamaCpp, "{family:?}");
            assert!(d.reason.contains("not verified for this model family"));
        }
        // A known family with a different architecture.
        let d = on(Facts {
            architecture: Some("bailingmoe2"),
            ..facts()
        });
        assert_eq!(d.kind, EngineKind::LlamaCpp);
        assert!(d.reason.contains("bailingmoe2"), "{}", d.reason);
        assert_eq!(
            on(Facts {
                architecture: None,
                ..facts()
            })
            .kind,
            EngineKind::LlamaCpp
        );
    }

    #[test]
    fn llm_engine_ik_skips_the_family_check_but_not_the_hardware_checks() {
        let forced = Facts {
            choice: EngineChoice::Ik,
            family: Family::Qwen35,
            architecture: Some("qwen35"),
            ..facts()
        };
        assert_eq!(kind(&forced), EngineKind::Ik);
        assert_eq!(decide(&forced).reason, "LLM_ENGINE=ik");
        assert_eq!(
            kind(&Facts {
                device: "Vulkan0",
                ..forced.clone()
            }),
            EngineKind::LlamaCpp
        );
        assert_eq!(
            kind(&Facts {
                cpu_avx2_fma: false,
                ..forced.clone()
            }),
            EngineKind::LlamaCpp
        );
        assert_eq!(
            kind(&Facts {
                ik_banned: true,
                ..forced
            }),
            EngineKind::LlamaCpp
        );
    }

    #[test]
    fn a_model_that_is_not_downloaded_starts_on_llamacpp_and_moves_to_ik() {
        let d = decide(&Facts {
            model_on_disk: false,
            architecture: None,
            ..facts()
        });
        assert_eq!(d.kind, EngineKind::LlamaCpp);
        assert!(d.download_first);
        // No download first for a family that ik does not run in auto mode.
        let d = decide(&Facts {
            model_on_disk: false,
            architecture: None,
            family: Family::Granite4,
            ..facts()
        });
        assert!(!d.download_first);
        // Forced ik also downloads with llama.cpp.
        let d = decide(&Facts {
            choice: EngineChoice::Ik,
            model_on_disk: false,
            architecture: None,
            family: Family::Granite4,
            ..facts()
        });
        assert!(d.download_first && d.kind == EngineKind::LlamaCpp);
    }

    fn outcome(ended: Ended, healthy: bool, secs: u64) -> Option<RunOutcome> {
        Some(RunOutcome {
            ended,
            healthy,
            ran: Duration::from_secs(secs),
        })
    }

    #[test]
    fn two_failed_starts_of_ik_ban_it() {
        let mut c = Chooser::default();
        c.note(EngineKind::Ik, None);
        c.note(EngineKind::Ik, outcome(Ended::Exited, false, 1));
        assert!(!c.banned(), "one failure is not enough");
        c.note(EngineKind::Ik, outcome(Ended::Exited, false, 1));
        assert!(c.banned());
    }

    #[test]
    fn a_long_healthy_run_clears_the_failures() {
        let mut c = Chooser::default();
        c.note(EngineKind::Ik, outcome(Ended::Exited, false, 1));
        c.note(EngineKind::Ik, outcome(Ended::Hung, true, 3600));
        c.note(EngineKind::Ik, outcome(Ended::Exited, false, 1));
        assert!(!c.banned());
    }

    #[test]
    fn a_crash_soon_after_the_first_answer_is_a_failure() {
        let mut c = Chooser::default();
        c.note(EngineKind::Ik, outcome(Ended::Exited, true, 30));
        c.note(EngineKind::Ik, outcome(Ended::Hung, true, 90));
        assert!(c.banned());
    }

    #[test]
    fn runs_of_llamacpp_and_replaced_runs_do_not_count() {
        let mut c = Chooser::default();
        for _ in 0..5 {
            c.note(EngineKind::LlamaCpp, outcome(Ended::Exited, false, 1));
            c.note(EngineKind::Ik, outcome(Ended::Replaced, true, 1));
        }
        assert!(!c.banned());
    }

    #[test]
    fn the_active_engine_is_shared() {
        let a = ActiveEngine::new(EngineKind::LlamaCpp);
        let b = a.clone();
        assert_eq!(b.get(), EngineKind::LlamaCpp);
        a.set(EngineKind::Ik);
        assert_eq!(b.get(), EngineKind::Ik);
    }

    const CPUINFO: &str = "processor\t: 0\nphysical id\t: 0\ncore id\t\t: 0\nflags\t\t: fpu sse2 avx avx2 fma bmi2\n\n\
        processor\t: 1\nphysical id\t: 0\ncore id\t\t: 1\nflags\t\t: fpu sse2 avx avx2 fma bmi2\n\n\
        processor\t: 2\nphysical id\t: 0\ncore id\t\t: 0\nflags\t\t: fpu sse2 avx avx2 fma bmi2\n\n\
        processor\t: 3\nphysical id\t: 0\ncore id\t\t: 1\nflags\t\t: fpu sse2 avx avx2 fma bmi2\n";

    #[test]
    fn cpu_flags() {
        assert!(has_avx2_fma(CPUINFO));
        assert!(!has_avx2_fma("flags : sse2 avx fma"));
        assert!(!has_avx2_fma("flags : sse2 avx2"));
        // Words must match whole: "avx2" is not "avx2_vnni".
        assert!(!has_avx2_fma("flags : avx2_vnni fma_x"));
        assert!(!has_avx2_fma(""));
    }

    #[test]
    fn threads_follow_the_physical_cores_up_to_eight() {
        assert_eq!(physical_cores(CPUINFO), Some(2), "4 threads on 2 cores");
        assert_eq!(ik_threads(CPUINFO, 4), 2);
        assert_eq!(ik_threads("", 6), 6, "no core list: the logical count");
        assert_eq!(ik_threads("", 64), 8);
        assert_eq!(ik_threads("", 0), 1);
        let many: String = (0..16)
            .map(|i| format!("processor : {i}\nphysical id : 0\ncore id : {i}\n\n"))
            .collect();
        assert_eq!(physical_cores(&many), Some(16));
        assert_eq!(ik_threads(&many, 16), 8);
        assert_eq!(ik_threads(&many, 4), 4, "a process pinned to 4 CPUs");
    }
}
