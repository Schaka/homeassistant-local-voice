//! One GPU job at a time, and a restart when the GPU is lost.
//!
//! STT (parakeet.cpp in this process) and TTS (audiocpp_server) have their own
//! Vulkan contexts. On small GPUs concurrent kernels from both can trip a GPU
//! reset, so every STT and TTS call holds this lock. All GPU work starts in
//! this process, so an in-process lock is enough.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;

/// Signs of a dead GPU (amdgpu reset, VK_ERROR_DEVICE_LOST). A timeout of the
/// TTS engine is also a sign: a GPU that stops running kernels leaves
/// audiocpp_server waiting forever. After three of these signs, neither engine
/// recovers on its own.
const FAULT_SIGNS: &[&str] = &[
    "device lost",
    "devicelost",
    "context is lost",
    "http error 500",
    "timed out",
    "timeout",
];
const FAULT_THRESHOLD: u32 = 3;
const LOCK_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone)]
pub struct Gpu {
    lock: Arc<Mutex<()>>,
    faults: Arc<AtomicU32>,
    lost: CancellationToken,
}

impl Gpu {
    /// `lost` fires after 3 GPU faults in a row. The server then exits with an
    /// error, and the restart policy of the container starts it fresh.
    pub fn new(lost: CancellationToken) -> Self {
        Self {
            lock: Arc::new(Mutex::new(())),
            faults: Arc::new(AtomicU32::new(0)),
            lost,
        }
    }

    pub async fn acquire(&self) -> anyhow::Result<OwnedMutexGuard<()>> {
        tokio::time::timeout(LOCK_TIMEOUT, self.lock.clone().lock_owned())
            .await
            .map_err(|_| anyhow::anyhow!("GPU busy (previous STT/TTS request still running)"))
    }

    pub fn success(&self) {
        let old = self.faults.swap(0, Ordering::Relaxed);
        if old > 0 {
            tracing::info!("GPU recovered (cleared {old} faults)");
        }
    }

    pub fn failure(&self, err: &anyhow::Error) {
        let text = format!("{err:#}").to_lowercase();
        if !FAULT_SIGNS.iter().any(|s| text.contains(s)) {
            tracing::warn!("non-GPU failure (not counted): {err:#}");
            return;
        }
        let count = self.faults.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::warn!("GPU fault {count}/{FAULT_THRESHOLD}: {err:#}");
        if count >= FAULT_THRESHOLD {
            tracing::error!("GPU appears lost after {count} faults, restarting");
            self.lost.cancel();
        }
    }
}

/// Names of the Vulkan GPUs (`Vulkan0`, ...) that ggml finds. Blocking.
///
/// It asks `llama-server --list-devices`, which uses the same ggml Vulkan
/// backend as parakeet.cpp and audio.cpp. Mesa's CPU driver (llvmpipe) is not
/// in the list, because ggml skips CPU devices. An empty list means: run STT
/// and TTS on the CPU.
pub fn vulkan_devices(llama_server: &std::path::Path) -> Vec<String> {
    match std::process::Command::new(llama_server)
        .arg("--list-devices")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    {
        Ok(out) => parse_device_list(&String::from_utf8_lossy(&out.stdout)),
        Err(err) => {
            tracing::warn!("cannot list GPUs with {}: {err}", llama_server.display());
            Vec::new()
        }
    }
}

/// Parses lines like `  Vulkan0: Intel(R) UHD Graphics 730 (15955 MiB, 11874 MiB free)`.
pub fn parse_device_list(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| line.trim().split_once(':'))
        .map(|(name, _)| name.trim())
        .filter(|name| name.starts_with("Vulkan") && name[6..].chars().all(|c| c.is_ascii_digit()))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_list() {
        let gpu = "Available devices:\n  Vulkan0: Intel(R) UHD Graphics 730 (ADL-S GT1) (15955 MiB, 11874 MiB free)\n";
        assert_eq!(parse_device_list(gpu), ["Vulkan0"]);
        assert!(parse_device_list("Available devices:\n  (none)\n").is_empty());
    }

    #[test]
    fn three_gpu_faults_trigger_a_restart() {
        let lost = CancellationToken::new();
        let gpu = Gpu::new(lost.clone());
        gpu.failure(&anyhow::anyhow!(
            "error sending request for url (http://127.0.0.1:8100/v1/audio/speech): operation timed out"
        ));
        gpu.failure(&anyhow::anyhow!("vk: Device Lost"));
        assert!(!lost.is_cancelled());
        gpu.failure(&anyhow::anyhow!("HTTP error 500: boom"));
        assert!(lost.is_cancelled());
    }

    #[test]
    fn success_clears_the_fault_count() {
        let lost = CancellationToken::new();
        let gpu = Gpu::new(lost.clone());
        gpu.failure(&anyhow::anyhow!("vk: Device Lost"));
        gpu.failure(&anyhow::anyhow!("vk: Device Lost"));
        gpu.success();
        gpu.failure(&anyhow::anyhow!("vk: Device Lost"));
        gpu.failure(&anyhow::anyhow!("vk: Device Lost"));
        assert!(!lost.is_cancelled());
    }

    #[test]
    fn other_errors_do_not_count() {
        let lost = CancellationToken::new();
        let gpu = Gpu::new(lost.clone());
        for _ in 0..10 {
            gpu.failure(&anyhow::anyhow!("no such file or directory"));
        }
        assert!(!lost.is_cancelled());
    }
}
