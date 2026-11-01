//! Runs the native engines (audiocpp_server, llama-server) as child processes
//! and restarts them when they exit. They listen on 127.0.0.1 only. Their
//! output goes to the container log as it is.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct ChildSpec {
    pub name: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub env_remove: Vec<String>,
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
                    tokio::select! {
                        status = child.wait() => match status {
                            Ok(s) => tracing::warn!("[{name}] exited ({s}), restarting in {RESTART_DELAY:?}"),
                            Err(e) => tracing::warn!("[{name}] wait failed ({e}), restarting in {RESTART_DELAY:?}"),
                        },
                        () = cancel.cancelled() => {
                            terminate(&name, &mut child).await;
                            return;
                        }
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
