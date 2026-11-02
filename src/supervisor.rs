//! Runs the native engines (audiocpp_server, llama-server) as child processes
//! and restarts them when they exit. They listen on 127.0.0.1 only. Their
//! output goes to the container log as it is.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// A health probe that runs at a fixed interval.
///
/// A child that exits is restarted at once. A child that hangs keeps its port
/// open and never exits, so only a probe can find it. This fault left the
/// voice pipeline silent for hours while Docker saw a healthy container.
#[derive(Debug, Clone)]
pub struct Health {
    /// `GET` this URL. Any 2xx answer means the child is alive.
    pub url: String,
    /// A start that has not answered yet is loading its model. Failed
    /// probes only count after this, so a first model download is not a hang.
    pub start_grace: Duration,
    pub interval: Duration,
    pub timeout: Duration,
    /// Consecutive failed probes before the child is restarted.
    pub failures: u32,
}

#[derive(Debug, Clone)]
pub struct ChildSpec {
    pub name: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub env_remove: Vec<String>,
    /// Counts the successful starts. Others watch it to notice a restart.
    pub starts: Arc<AtomicU64>,
    pub health: Option<Health>,
}

const RESTART_DELAY: Duration = Duration::from_secs(2);
const STOP_GRACE: Duration = Duration::from_secs(5);

async fn terminate(name: &str, child: &mut Child) {
    if let Some(id) = child.id() {
        let pid = Pid::from_raw(i32::try_from(id).unwrap_or(i32::MAX));
        let _ = kill(pid, Signal::SIGTERM);
        if tokio::time::timeout(STOP_GRACE, child.wait()).await.is_ok() {
            tracing::info!("[{name}] stopped");
            return;
        }
        tracing::warn!("[{name}] did not stop in {STOP_GRACE:?}, killing it");
    }
    let _ = child.kill().await;
}

/// Probes one child and cancels `fatal` when it stops answering.
async fn watch(name: String, health: Health, fatal: CancellationToken, cancel: CancellationToken) {
    let client = match reqwest::Client::builder().timeout(health.timeout).build() {
        Ok(client) => client,
        Err(err) => {
            tracing::error!("[{name}] watchdog disabled: {err}");
            return;
        }
    };
    let started = Instant::now();
    let mut seen_healthy = false;
    let mut failed = 0u32;
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(health.interval) => {}
        }
        if fatal.is_cancelled() {
            return;
        }
        let ok = client
            .get(&health.url)
            .send()
            .await
            .is_ok_and(|resp| resp.status().is_success());
        if ok {
            if !seen_healthy {
                tracing::info!("[{name}] answers on {} after {:?}", health.url, started.elapsed());
                seen_healthy = true;
            }
            failed = 0;
            continue;
        }
        if !seen_healthy && started.elapsed() < health.start_grace {
            tracing::debug!("[{name}] not ready yet on {}", health.url);
            continue;
        }
        failed += 1;
        tracing::warn!(
            "[{name}] no answer on {} (attempt {failed}/{})",
            health.url,
            health.failures
        );
        if failed >= health.failures {
            tracing::error!("[{name}] appears hung, restarting it");
            fatal.cancel();
            return;
        }
    }
}

/// Starts the child and keeps it running until `cancel` fires.
pub fn spawn(spec: ChildSpec, cancel: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(async move {
        let name = spec.name.clone();
        while !cancel.is_cancelled() {
            tracing::info!("[{name}] starting: {} {}", spec.program.display(), spec.args.join(" "));
            let mut cmd = Command::new(&spec.program);
            cmd.args(&spec.args)
                .envs(spec.env.iter().map(|(k, v)| (k, v)))
                .stdin(Stdio::null())
                .kill_on_drop(true);
            for key in &spec.env_remove {
                cmd.env_remove(key);
            }
            match cmd.spawn() {
                Ok(mut child) => {
                    spec.starts.fetch_add(1, Ordering::SeqCst);
                    let fatal = CancellationToken::new();
                    let watcher = spec
                        .health
                        .clone()
                        .map(|health| tokio::spawn(watch(name.clone(), health, fatal.clone(), cancel.clone())));
                    tokio::select! {
                        status = child.wait() => match status {
                            Ok(s) => tracing::warn!("[{name}] exited ({s}), restarting in {RESTART_DELAY:?}"),
                            Err(e) => tracing::warn!("[{name}] wait failed ({e}), restarting in {RESTART_DELAY:?}"),
                        },
                        () = cancel.cancelled() => {
                            if let Some(w) = &watcher {
                                w.abort();
                            }
                            terminate(&name, &mut child).await;
                            return;
                        }
                        () = fatal.cancelled() => {
                            terminate(&name, &mut child).await;
                        }
                    }
                    if let Some(w) = watcher {
                        w.abort();
                    }
                }
                Err(err) => tracing::error!("[{name}] cannot start: {err}"),
            }
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(RESTART_DELAY) => {}
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_hung_child_is_restarted() {
        let cancel = CancellationToken::new();
        let spec = ChildSpec {
            name: "sleeper".into(),
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "sleep 100".into()],
            env: vec![],
            env_remove: vec![],
            starts: Default::default(),
            health: Some(Health {
                // Nothing listens there: every probe fails at once.
                url: "http://127.0.0.1:1/health".into(),
                start_grace: Duration::ZERO,
                interval: Duration::from_millis(50),
                timeout: Duration::from_millis(200),
                failures: 2,
            }),
        };
        let starts = spec.starts.clone();
        let task = spawn(spec, cancel.clone());
        let deadline = Instant::now() + Duration::from_secs(10);
        while starts.load(Ordering::SeqCst) < 2 {
            assert!(Instant::now() < deadline, "the hung child was not restarted");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        cancel.cancel();
        assert!(tokio::time::timeout(Duration::from_secs(5), task).await.is_ok());
    }
}
