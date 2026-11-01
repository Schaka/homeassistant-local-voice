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

/// Signatures of a dead GPU (amdgpu reset, VK_ERROR_DEVICE_LOST). When they
/// pile up, neither engine recovers on its own.
const FAULT_SIGNS: &[&str] = &["device lost", "devicelost", "context is lost", "http error 500"];
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_gpu_faults_trigger_a_restart() {
        let lost = CancellationToken::new();
        let gpu = Gpu::new(lost.clone());
        gpu.failure(&anyhow::anyhow!("timeout"));
        gpu.failure(&anyhow::anyhow!("vk: Device Lost"));
        gpu.failure(&anyhow::anyhow!("HTTP error 500: boom"));
        assert!(!lost.is_cancelled());
        gpu.success();
        for _ in 0..3 {
            gpu.failure(&anyhow::anyhow!("ErrorDeviceLost"));
        }
        assert!(lost.is_cancelled());
    }
}
