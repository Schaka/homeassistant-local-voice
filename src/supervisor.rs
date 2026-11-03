//! Runs the native engines (audiocpp_server, llama-server) as child processes
//! and restarts them when they exit. They listen on 127.0.0.1 only. Their
//! output goes to the log of the hub with a level for each line ([`output`]).
//!
//! A [`SpecSource`] makes the spec for each start. The LLM uses this to choose
//! its engine anew at every start, with the outcome of the last run in hand.

use std::future::Future;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

mod output;

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
    /// The time between two probes once the child answered. Until then, the
    /// probe runs every [`STARTUP_POLL`], so the log shows the real start time.
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
    /// Stop the child at its first good health answer, so that the next spec
    /// replaces it. The LLM uses this after the first download of a model.
    pub restart_when_healthy: bool,
}

/// How a run of a child ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The process exited by itself.
    Exited,
    /// The probes failed and the supervisor stopped it.
    Hung,
    /// The spec asked for a restart at the first good answer.
    Replaced,
    /// The process did not start.
    SpawnFailed,
}

#[derive(Debug, Clone, Copy)]
pub struct RunOutcome {
    pub ended: Ended,
    /// The child answered a health probe at least once.
    pub healthy: bool,
    pub ran: Duration,
}

/// Makes the spec for each start of a child.
pub trait SpecSource: Send + 'static {
    /// `last` is the outcome of the run before. None: the first start.
    fn next(&mut self, last: Option<RunOutcome>) -> impl Future<Output = ChildSpec> + Send;
}

/// The same spec for every start.
struct Fixed(ChildSpec);

impl SpecSource for Fixed {
    async fn next(&mut self, _last: Option<RunOutcome>) -> ChildSpec {
        self.0.clone()
    }
}

/// What the watcher tells the supervisor about the current run.
#[derive(Default)]
struct RunState {
    healthy: AtomicBool,
    replace: AtomicBool,
}

const RESTART_DELAY: Duration = Duration::from_secs(2);
const STOP_GRACE: Duration = Duration::from_secs(5);
/// The probe interval while a child loads its model.
const STARTUP_POLL: Duration = Duration::from_secs(1);
/// The time that the output readers get after the end of a child, so that
/// its last lines reach the tail.
const OUTPUT_DRAIN: Duration = Duration::from_secs(1);

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
async fn watch(
    name: String,
    health: Health,
    state: Arc<RunState>,
    restart_when_healthy: bool,
    fatal: CancellationToken,
    cancel: CancellationToken,
) {
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
        let pause = if seen_healthy || started.elapsed() >= health.start_grace {
            health.interval
        } else {
            health.interval.min(STARTUP_POLL)
        };
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(pause) => {}
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
                tracing::info!("[{name}] ready after {:.1} s", started.elapsed().as_secs_f64());
                seen_healthy = true;
                state.healthy.store(true, Ordering::SeqCst);
                if restart_when_healthy {
                    tracing::info!("[{name}] is ready, restarting it to apply the next start settings");
                    state.replace.store(true, Ordering::SeqCst);
                    fatal.cancel();
                    return;
                }
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
    spawn_source(Fixed(spec), cancel)
}

/// Like [`spawn`], with a new spec for each start.
pub fn spawn_source(mut source: impl SpecSource, cancel: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut last: Option<RunOutcome> = None;
        while !cancel.is_cancelled() {
            let spec = source.next(last).await;
            let name = spec.name.clone();
            tracing::info!("[{name}] starting: {} {}", spec.program.display(), spec.args.join(" "));
            let mut cmd = Command::new(&spec.program);
            cmd.args(&spec.args)
                .envs(spec.env.iter().map(|(k, v)| (k, v)))
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            for key in &spec.env_remove {
                cmd.env_remove(key);
            }
            let began = Instant::now();
            let state = Arc::new(RunState::default());
            let mut ended = Ended::SpawnFailed;
            match cmd.spawn() {
                Ok(mut child) => {
                    spec.starts.fetch_add(1, Ordering::SeqCst);
                    let tail = output::Tail::default();
                    let readers: Vec<JoinHandle<()>> = [
                        child
                            .stdout
                            .take()
                            .map(|out| output::forward(name.clone(), out, tail.clone())),
                        child
                            .stderr
                            .take()
                            .map(|err| output::forward(name.clone(), err, tail.clone())),
                    ]
                    .into_iter()
                    .flatten()
                    .collect();
                    let fatal = CancellationToken::new();
                    let watcher = spec.health.clone().map(|health| {
                        tokio::spawn(watch(
                            name.clone(),
                            health,
                            state.clone(),
                            spec.restart_when_healthy,
                            fatal.clone(),
                            cancel.clone(),
                        ))
                    });
                    tokio::select! {
                        status = child.wait() => {
                            ended = Ended::Exited;
                            match status {
                                Ok(s) => tracing::warn!("[{name}] exited ({s}), restarting in {RESTART_DELAY:?}"),
                                Err(e) => tracing::warn!("[{name}] wait failed ({e}), restarting in {RESTART_DELAY:?}"),
                            }
                        },
                        () = cancel.cancelled() => {
                            if let Some(w) = &watcher {
                                w.abort();
                            }
                            terminate(&name, &mut child).await;
                            return;
                        }
                        () = fatal.cancelled() => {
                            ended = if state.replace.load(Ordering::SeqCst) { Ended::Replaced } else { Ended::Hung };
                            terminate(&name, &mut child).await;
                        }
                    }
                    if let Some(w) = watcher {
                        w.abort();
                    }
                    // A grandchild that keeps a pipe open must not stop the restart.
                    for reader in readers {
                        let abort = reader.abort_handle();
                        if tokio::time::timeout(OUTPUT_DRAIN, reader).await.is_err() {
                            abort.abort();
                        }
                    }
                    if ended != Ended::Replaced {
                        tail.report(&name);
                    }
                }
                Err(err) => tracing::error!("[{name}] cannot start: {err}"),
            }
            last = Some(RunOutcome {
                ended,
                healthy: state.healthy.load(Ordering::SeqCst),
                ran: began.elapsed(),
            });
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
    use crate::llm::fake::{Fake, HealthMode};

    fn sh(script: &str) -> ChildSpec {
        ChildSpec {
            name: "test".into(),
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            env: vec![],
            env_remove: vec![],
            starts: Default::default(),
            health: None,
            restart_when_healthy: false,
        }
    }

    fn health(url: String, grace: Duration, failures: u32) -> Option<Health> {
        Some(Health {
            url,
            start_grace: grace,
            interval: Duration::from_millis(50),
            timeout: Duration::from_millis(300),
            failures,
        })
    }

    async fn wait_for(what: &str, secs: u64, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while !done() {
            assert!(Instant::now() < deadline, "timeout: {what}");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn stop(task: JoinHandle<()>, cancel: &CancellationToken) -> Duration {
        let begin = Instant::now();
        cancel.cancel();
        assert!(
            tokio::time::timeout(Duration::from_secs(10), task).await.is_ok(),
            "the supervisor did not stop"
        );
        begin.elapsed()
    }

    #[tokio::test]
    async fn a_hung_child_is_restarted() {
        let cancel = CancellationToken::new();
        let mut spec = sh("sleep 100");
        // Nothing listens there: every probe fails at once.
        spec.health = health("http://127.0.0.1:1/health".into(), Duration::ZERO, 2);
        let starts = spec.starts.clone();
        let task = spawn(spec, cancel.clone());
        wait_for("the hung child restarts", 10, || starts.load(Ordering::SeqCst) >= 2).await;
        stop(task, &cancel).await;
    }

    #[tokio::test]
    async fn a_child_that_exits_is_started_again_after_the_delay() {
        let cancel = CancellationToken::new();
        let spec = sh("exit 3");
        let starts = spec.starts.clone();
        let begin = Instant::now();
        let task = spawn(spec, cancel.clone());
        wait_for("second start", 10, || starts.load(Ordering::SeqCst) >= 2).await;
        let took = begin.elapsed();
        assert!(took >= RESTART_DELAY, "restarted after {took:?}, before the delay");
        assert!(
            took < RESTART_DELAY + Duration::from_secs(6),
            "restarted after {took:?}"
        );
        stop(task, &cancel).await;
    }

    #[tokio::test]
    async fn failed_probes_during_the_start_grace_do_not_count() {
        let cancel = CancellationToken::new();
        let mut spec = sh("sleep 100");
        spec.health = health("http://127.0.0.1:1/health".into(), Duration::from_secs(3600), 1);
        let starts = spec.starts.clone();
        let task = spawn(spec, cancel.clone());
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(starts.load(Ordering::SeqCst), 1, "a loading model is no hang");
        stop(task, &cancel).await;
    }

    #[tokio::test]
    async fn a_child_that_answers_stays_and_one_that_stops_answering_is_restarted() {
        let fake = Fake::default();
        let port = fake.spawn().await;
        let cancel = CancellationToken::new();
        let mut spec = sh("sleep 100");
        spec.health = health(format!("http://127.0.0.1:{port}/health"), Duration::ZERO, 3);
        let starts = spec.starts.clone();
        let task = spawn(spec, cancel.clone());
        tokio::time::sleep(Duration::from_millis(1000)).await;
        assert_eq!(starts.load(Ordering::SeqCst), 1, "a healthy child is not touched");
        // A busy model answers 503 now and then. Two failures in a row are not enough.
        fake.set_health(HealthMode::Loading);
        tokio::time::sleep(Duration::from_millis(110)).await;
        fake.set_health(HealthMode::Ok);
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(
            starts.load(Ordering::SeqCst),
            1,
            "the failure count starts again after an answer"
        );
        fake.set_health(HealthMode::Loading);
        wait_for("restart after three failed probes", 10, || {
            starts.load(Ordering::SeqCst) >= 2
        })
        .await;
        stop(task, &cancel).await;
    }

    #[tokio::test]
    async fn stop_sends_sigterm_and_waits_for_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let mark = dir.path().join("term");
        let ready = dir.path().join("ready");
        let script = format!(
            "trap 'echo term > {}; exit 0' TERM; echo up > {}; while :; do sleep 0.05; done",
            mark.display(),
            ready.display()
        );
        let cancel = CancellationToken::new();
        let task = spawn(sh(&script), cancel.clone());
        wait_for("the child runs", 5, || ready.exists()).await;
        let took = stop(task, &cancel).await;
        assert!(mark.exists(), "the child got SIGTERM and could clean up");
        assert!(took < Duration::from_secs(5), "stop took {took:?}");
    }

    #[tokio::test]
    async fn a_child_that_ignores_sigterm_is_killed_after_the_grace_time() {
        let dir = tempfile::tempdir().unwrap();
        let ready = dir.path().join("ready");
        let pidfile = dir.path().join("pid");
        let script = format!(
            "trap '' TERM; echo $$ > {}; echo up > {}; while :; do sleep 0.05; done",
            pidfile.display(),
            ready.display()
        );
        let cancel = CancellationToken::new();
        let task = spawn(sh(&script), cancel.clone());
        wait_for("the child runs", 5, || ready.exists()).await;
        let pid: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        let took = stop(task, &cancel).await;
        assert!(took >= STOP_GRACE - Duration::from_millis(200), "killed after {took:?}");
        assert!(took < STOP_GRACE + Duration::from_secs(6), "killed after {took:?}");
        // kill with no signal only tests that the process exists.
        let gone = (0..40).any(|_| {
            std::thread::sleep(Duration::from_millis(50));
            kill(Pid::from_raw(pid), None).is_err()
        });
        assert!(gone, "the child process is still alive");
    }

    #[tokio::test]
    async fn a_child_with_much_output_does_not_block_on_the_pipe() {
        let dir = tempfile::tempdir().unwrap();
        let done = dir.path().join("done");
        // 2 MB on each pipe, far more than a pipe buffer holds.
        let script = format!(
            "head -c 2000000 /dev/zero | tr '\\0' 'x'; head -c 2000000 /dev/zero | tr '\\0' 'y' >&2; echo ok > {}; sleep 100",
            done.display()
        );
        let cancel = CancellationToken::new();
        let task = spawn(sh(&script), cancel.clone());
        wait_for("the child writes all its output", 10, || done.exists()).await;
        stop(task, &cancel).await;
    }

    #[tokio::test]
    async fn environment_is_passed_and_removed() {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        assert!(!home.is_empty());
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let cancel = CancellationToken::new();
        let mut spec = sh(&format!(
            "echo \"${{FOO-unset}} ${{HOME-unset}}\" > {}; sleep 100",
            out.display()
        ));
        spec.env = vec![("FOO".into(), "bar".into())];
        spec.env_remove = vec!["HOME".into()];
        let task = spawn(spec, cancel.clone());
        wait_for("the child writes", 5, || out.exists()).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(std::fs::read_to_string(&out).unwrap().trim(), "bar unset");
        stop(task, &cancel).await;
    }

    #[tokio::test]
    async fn a_program_that_does_not_exist_is_retried_and_not_counted_as_a_start() {
        let cancel = CancellationToken::new();
        let mut spec = sh("true");
        spec.program = "/nonexistent/engine".into();
        let starts = spec.starts.clone();
        let task = spawn(spec, cancel.clone());
        tokio::time::sleep(Duration::from_millis(2600)).await;
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        assert!(!task.is_finished(), "the supervisor keeps trying");
        stop(task, &cancel).await;
    }

    #[tokio::test]
    async fn stop_during_the_restart_delay_ends_at_once() {
        let cancel = CancellationToken::new();
        let spec = sh("exit 0");
        let starts = spec.starts.clone();
        let task = spawn(spec, cancel.clone());
        wait_for("first start", 5, || starts.load(Ordering::SeqCst) >= 1).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let took = stop(task, &cancel).await;
        assert!(took < Duration::from_millis(500), "stop took {took:?}");
        assert_eq!(starts.load(Ordering::SeqCst), 1);
    }

    /// Records the outcomes that the supervisor hands to the spec source.
    struct Recorder {
        spec: ChildSpec,
        seen: Arc<std::sync::Mutex<Vec<Option<RunOutcome>>>>,
    }

    impl SpecSource for Recorder {
        async fn next(&mut self, last: Option<RunOutcome>) -> ChildSpec {
            self.seen.lock().unwrap().push(last);
            self.spec.clone()
        }
    }

    #[tokio::test]
    async fn the_source_sees_how_the_last_run_ended() {
        let cancel = CancellationToken::new();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let spec = sh("exit 3");
        let starts = spec.starts.clone();
        let task = spawn_source(
            Recorder {
                spec,
                seen: seen.clone(),
            },
            cancel.clone(),
        );
        wait_for("second start", 10, || starts.load(Ordering::SeqCst) >= 2).await;
        stop(task, &cancel).await;
        let seen = seen.lock().unwrap();
        assert!(seen[0].is_none(), "the first start has no history");
        let second = seen[1].expect("the second start knows the first run");
        assert_eq!(second.ended, Ended::Exited);
        assert!(!second.healthy);
        assert!(second.ran < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn a_hung_run_and_a_failed_spawn_are_told_apart() {
        let cancel = CancellationToken::new();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut spec = sh("sleep 100");
        spec.health = health("http://127.0.0.1:1/health".into(), Duration::ZERO, 2);
        let starts = spec.starts.clone();
        let task = spawn_source(
            Recorder {
                spec,
                seen: seen.clone(),
            },
            cancel.clone(),
        );
        wait_for("restart after a hang", 10, || starts.load(Ordering::SeqCst) >= 2).await;
        stop(task, &cancel).await;
        assert_eq!(seen.lock().unwrap()[1].unwrap().ended, Ended::Hung);

        let cancel = CancellationToken::new();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut spec = sh("true");
        spec.program = "/nonexistent/engine".into();
        let task = spawn_source(
            Recorder {
                spec,
                seen: seen.clone(),
            },
            cancel.clone(),
        );
        wait_for("second attempt", 10, || seen.lock().unwrap().len() >= 2).await;
        stop(task, &cancel).await;
        assert_eq!(seen.lock().unwrap()[1].unwrap().ended, Ended::SpawnFailed);
    }

    #[tokio::test]
    async fn restart_when_healthy_replaces_the_child_after_its_first_good_answer() {
        let fake = Fake::default();
        let port = fake.spawn().await;
        let cancel = CancellationToken::new();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut spec = sh("sleep 100");
        spec.health = health(format!("http://127.0.0.1:{port}/health"), Duration::from_secs(60), 3);
        spec.restart_when_healthy = true;
        let starts = spec.starts.clone();
        let task = spawn_source(
            Recorder {
                spec,
                seen: seen.clone(),
            },
            cancel.clone(),
        );
        wait_for("replacement", 10, || starts.load(Ordering::SeqCst) >= 2).await;
        stop(task, &cancel).await;
        let second = seen.lock().unwrap()[1].unwrap();
        assert_eq!(second.ended, Ended::Replaced);
        assert!(second.healthy);
    }
}
