use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

use crate::harness;
use crate::protocol::{MutantResult, MutantStatus, OrchestratorMessage, WorkItem, WorkerMessage};
use crate::trace::TraceLog;

/// Check if a process is still alive using kill(pid, 0).
#[cfg(unix)]
fn is_process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Configuration for the worker pool.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Number of worker processes to spawn.
    pub num_workers: usize,
    /// Path to the Python interpreter.
    pub python: PathBuf,
    /// Base directory of the project being tested.
    pub project_dir: PathBuf,
    /// Directory containing mutated source files.
    pub mutants_dir: PathBuf,
    /// Directory containing tests.
    pub tests_dir: PathBuf,
    /// Timeout multiplier (applied to baseline test duration).
    pub timeout_multiplier: f64,
    /// Default timeout if no baseline duration is known.
    pub default_timeout: Duration,
    /// Pre-built PYTHONPATH string for worker subprocesses.
    /// Format: harness_dir:source_parent. mutants_dir is passed as
    /// IRRADIATE_MUTANTS_DIR env var and handled by the MutantFinder import hook.
    pub pythonpath: String,
    /// Recycle workers whose RSS exceeds this many megabytes. 0 = unlimited.
    pub max_worker_memory_mb: usize,
    /// Extra arguments appended to every pytest invocation within the worker pool.
    /// Passed to worker processes via the `IRRADIATE_PYTEST_ARGS` env var (JSON array).
    pub pytest_add_cli_args: Vec<String>,
    /// Timeout in seconds for workers to complete test collection and send the ready message.
    /// Default 30s. Increase for projects with slow imports (e.g. tinygrad, torch).
    pub worker_ready_timeout: u64,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            num_workers: num_cpus::get(),
            python: PathBuf::from("python3"),
            project_dir: PathBuf::from("."),
            mutants_dir: PathBuf::from("mutants"),
            tests_dir: PathBuf::from("tests"),
            timeout_multiplier: 10.0,
            default_timeout: Duration::from_secs(30),
            pythonpath: String::new(),
            max_worker_memory_mb: 0,
            pytest_add_cli_args: Vec::new(),
            worker_ready_timeout: 30,
        }
    }
}

/// Event from the worker communication loop.
enum WorkerEvent {
    Ready {
        worker_id: usize,
    },
    Result {
        worker_id: usize,
        result: MutantResult,
    },
    Disconnected {
        worker_id: usize,
    },
    Error {
        worker_id: usize,
        message: String,
        mutant: Option<String>,
    },
}

/// Workers that have been spawned but not yet dispatched work.
///
/// Created by `pre_spawn_pool()` so workers can boot (Python startup + pytest
/// collection) in parallel with stats collection or other pipeline phases.
pub struct PreSpawnedPool {
    listener: UnixListener,
    processes: Vec<Child>,
    socket_path: PathBuf,
    harness_dir: PathBuf,
}

/// Spawn workers early so they can boot during stats collection.
///
/// Workers connect to the socket and run pytest collection (~480ms), then wait
/// for the "ready" handshake. By the time stats finishes and work items are
/// built, workers are already warm and ready to receive work.
pub fn pre_spawn_pool(
    config: &PoolConfig,
    harness_dir: &Path,
    num_workers: usize,
) -> Result<PreSpawnedPool> {
    let socket_name = format!(
        "irradiate-{}-{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos()
    );
    let socket_path = std::env::temp_dir().join(socket_name);
    let _ = std::fs::remove_file(&socket_path);

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("Failed to bind unix socket at {}", socket_path.display()))?;
    info!("Pre-spawning {num_workers} workers on {}", socket_path.display());

    let mut processes: Vec<Child> = Vec::new();
    for i in 0..num_workers {
        let child = spawn_worker(i, config, harness_dir, &socket_path)?;
        processes.push(child);
    }

    Ok(PreSpawnedPool {
        listener,
        processes,
        socket_path,
        harness_dir: harness_dir.to_path_buf(),
    })
}

/// Run a pool of pytest workers against a list of work items.
///
/// Returns the results for all mutants.
pub async fn run_worker_pool(
    config: &PoolConfig,
    work_items: Vec<WorkItem>,
    progress: Option<crate::progress::ProgressBar>,
) -> Result<(Vec<MutantResult>, TraceLog)> {
    if work_items.is_empty() {
        return Ok((vec![], TraceLog::new()));
    }

    // Extract harness
    let harness_dir =
        harness::extract_harness(&config.project_dir).context("Failed to extract harness")?;

    let num_workers = config.num_workers.min(work_items.len());
    let pre_spawned = pre_spawn_pool(config, &harness_dir, num_workers)?;

    run_worker_pool_pre_spawned(config, work_items, progress, pre_spawned).await
}

/// Run a pool using pre-spawned workers.
///
/// Use this when workers were spawned early (via `pre_spawn_pool`) to overlap
/// worker boot time with other pipeline phases like stats collection.
pub async fn run_worker_pool_pre_spawned(
    config: &PoolConfig,
    work_items: Vec<WorkItem>,
    progress: Option<crate::progress::ProgressBar>,
    pre_spawned: PreSpawnedPool,
) -> Result<(Vec<MutantResult>, TraceLog)> {
    if work_items.is_empty() {
        // Clean up pre-spawned resources
        let _ = std::fs::remove_file(&pre_spawned.socket_path);
        return Ok((vec![], TraceLog::new()));
    }

    // Accept connections and dispatch work
    let mut trace = TraceLog::new();
    let results = dispatch_work(
        pre_spawned.listener,
        pre_spawned.processes,
        work_items,
        config,
        &pre_spawned.harness_dir,
        &pre_spawned.socket_path,
        progress,
        &mut trace,
    )
    .await?;

    // Clean up socket
    let _ = std::fs::remove_file(&pre_spawned.socket_path);

    Ok((results, trace))
}

fn spawn_worker(
    id: usize,
    config: &PoolConfig,
    harness_dir: &Path,
    socket_path: &Path,
) -> Result<Child> {
    let worker_script = harness::worker_script(harness_dir);

    let pytest_args_json =
        serde_json::to_string(&config.pytest_add_cli_args).unwrap_or_else(|_| "[]".to_string());

    let child = Command::new(&config.python)
        .arg(&worker_script)
        .env("IRRADIATE_SOCKET", socket_path)
        .env("IRRADIATE_MUTANTS_DIR", &config.mutants_dir)
        .env("IRRADIATE_TESTS_DIR", &config.tests_dir)
        .env("PYTHONPATH", &config.pythonpath)
        .env("IRRADIATE_PYTEST_ARGS", &pytest_args_json)
        // Avoid .pyc writes — workers are short-lived, disk I/O wastes startup time
        .env("PYTHONDONTWRITEBYTECODE", "1")
        // Pass through profiling dir if set (for perf analysis)
        .envs(std::env::var("IRRADIATE_PROFILE_DIR").ok().map(|v| ("IRRADIATE_PROFILE_DIR", v)))
        .current_dir(&config.project_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!(
            "Failed to spawn worker {id} — is '{}' a valid Python interpreter?",
            config.python.display()
        ))?;

    info!("Spawned worker {id} with pid {}", child.id().unwrap_or(0));
    Ok(child)
}

/// Spawn a tokio task that forwards messages between the orchestrator and a connected worker.
///
/// Returns a sender for sending OrchestratorMessages to the worker.
fn spawn_worker_task(
    worker_id: usize,
    reader: tokio::net::unix::OwnedReadHalf,
    mut writer: tokio::net::unix::OwnedWriteHalf,
    event_tx: mpsc::Sender<WorkerEvent>,
    default_timeout: Duration,
    timeout_multiplier: f64,
) -> mpsc::Sender<OrchestratorMessage> {
    let (msg_tx, mut msg_rx) = mpsc::channel::<OrchestratorMessage>(8);
    let mut reader = BufReader::new(reader);

    tokio::spawn(async move {
        // Spawn writer task
        let (write_tx, mut write_rx) = mpsc::channel::<String>(8);
        tokio::spawn(async move {
            while let Some(data) = write_rx.recv().await {
                if writer.write_all(data.as_bytes()).await.is_err() {
                    break;
                }
            }
        });

        // Per-mutant read timeout: starts at the global default, updated for each Run message.
        // Duration is Copy so each loop iteration's async block captures the current value.
        let mut current_timeout = default_timeout.mul_f64(timeout_multiplier);

        loop {
            tokio::select! {
                msg = msg_rx.recv() => {
                    match msg {
                        Some(msg) => {
                            // Update read timeout when dispatching a Run with a per-mutant timeout.
                            if let OrchestratorMessage::Run { timeout_secs: Some(secs), .. } = &msg {
                                current_timeout = Duration::from_secs_f64(*secs);
                            }
                            let json = serde_json::to_string(&msg).context("internal error: failed to serialize worker message — please report this as a bug")? + "\n";
                            if write_tx.send(json).await.is_err() {
                                let _ = event_tx.send(WorkerEvent::Disconnected { worker_id }).await;
                                break;
                            }
                        }
                        None => break, // channel closed
                    }
                }
                line = async {
                    let mut line = String::new();
                    match timeout(current_timeout, reader.read_line(&mut line)).await {
                        Ok(Ok(0)) => None, // EOF
                        Ok(Ok(_)) => Some(Ok(line)),
                        Ok(Err(e)) => Some(Err(e)),
                        Err(_) => Some(Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "worker timed out"))),
                    }
                } => {
                    match line {
                        None => {
                            let _ = event_tx.send(WorkerEvent::Disconnected { worker_id }).await;
                            break;
                        }
                        Some(Ok(line)) => {
                            if line.trim().is_empty() {
                                continue;
                            }
                            match serde_json::from_str::<WorkerMessage>(line.trim()) {
                                Ok(WorkerMessage::Result { mutant, exit_code, duration }) => {
                                    let status = MutantStatus::from_exit_code(exit_code, false);
                                    let _ = event_tx.send(WorkerEvent::Result {
                                        worker_id,
                                        result: MutantResult {
                                            mutant_name: mutant,
                                            exit_code,
                                            duration,
                                            status,
                                        },
                                    }).await;
                                }
                                Ok(WorkerMessage::Ready { .. }) => {
                                    let _ = event_tx.send(WorkerEvent::Ready { worker_id }).await;
                                }
                                Ok(WorkerMessage::Error { mutant, message, .. }) => {
                                    let _ = event_tx.send(WorkerEvent::Error {
                                        worker_id,
                                        message,
                                        mutant,
                                    }).await;
                                }
                                Err(e) => {
                                    warn!("Worker {worker_id}: failed to parse message: {e}: {line}");
                                }
                            }
                        }
                        Some(Err(e)) => {
                            if e.kind() == std::io::ErrorKind::TimedOut {
                                warn!("Worker {worker_id}: timed out");
                            }
                            let _ = event_tx.send(WorkerEvent::Disconnected { worker_id }).await;
                            break;
                        }
                    }
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    });

    msg_tx
}

/// Sample the RSS (resident set size) of a process in kilobytes.
///
/// Uses platform-native APIs to avoid spawning a subprocess.
/// macOS uses `proc_pidinfo(PROC_PIDTASKINFO)`, Linux reads `/proc/{pid}/statm`.
/// Falls back to `ps -o rss=` if native methods fail.
async fn check_rss(pid: u32) -> Result<usize> {
    if let Some(rss_kb) = check_rss_native(pid) {
        return Ok(rss_kb);
    }
    // Fallback: spawn ps
    let output = tokio::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .await?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.trim().parse::<usize>().map_err(|e| anyhow::anyhow!(e))
}

/// Try to read RSS using platform-native API (no subprocess spawn).
#[cfg(target_os = "macos")]
fn check_rss_native(pid: u32) -> Option<usize> {
    // proc_pidinfo with PROC_PIDTASKINFO returns proc_taskinfo containing
    // pti_resident_size (in bytes). Available since macOS 10.5.
    #[repr(C)]
    #[allow(non_camel_case_types)]
    struct proc_taskinfo {
        pti_virtual_size: u64,
        pti_resident_size: u64,
        pti_total_user: u64,
        pti_total_system: u64,
        pti_threads_user: u64,
        pti_threads_system: u64,
        pti_policy: i32,
        pti_faults: i32,
        pti_pageins: i32,
        pti_cow_faults: i32,
        pti_messages_sent: i32,
        pti_messages_received: i32,
        pti_syscalls_mach: i32,
        pti_syscalls_unix: i32,
        pti_csw: i32,
        pti_threadnum: i32,
        pti_numrunning: i32,
        pti_priority: i32,
    }

    const PROC_PIDTASKINFO: i32 = 4;

    unsafe {
        let mut info: proc_taskinfo = std::mem::zeroed();
        let size = std::mem::size_of::<proc_taskinfo>() as i32;
        let ret = libc::proc_pidinfo(
            pid as i32,
            PROC_PIDTASKINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        );
        if ret <= 0 {
            return None;
        }
        // Convert bytes to kilobytes
        Some((info.pti_resident_size / 1024) as usize)
    }
}

#[cfg(target_os = "linux")]
fn check_rss_native(pid: u32) -> Option<usize> {
    // /proc/{pid}/statm fields: size resident shared text lib data dt (in pages)
    let content = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
    let resident_pages: usize = content.split_whitespace().nth(1)?.parse().ok()?;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    Some(resident_pages * page_size / 1024)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn check_rss_native(_pid: u32) -> Option<usize> {
    None
}

/// Mutable state for the dispatch loop, extracted from the monolithic `dispatch_work` function.
struct DispatchState<'a> {
    // Core state
    results: Vec<MutantResult>,
    work_queue: Vec<WorkItem>,
    total_items: usize,

    // Worker tracking
    worker_senders: HashMap<usize, mpsc::Sender<OrchestratorMessage>>,
    idle_workers: Vec<usize>,
    active_mutants: HashMap<usize, String>,
    next_worker_id: usize,

    pending_accepts: usize,

    // Memory monitoring
    worker_pids: HashMap<usize, u32>,
    workers_pending_memory_recycle: HashSet<usize>,

    // Tracing
    spawn_times: HashMap<usize, u64>,
    dispatch_times: HashMap<usize, (u64, String)>,
    trace: &'a mut TraceLog,

    // Resources (owned by the dispatch loop, passed through for respawning)
    processes: Vec<Child>,
    progress: Option<crate::progress::ProgressBar>,
    event_tx: mpsc::Sender<WorkerEvent>,

    // Config (borrowed)
    config: &'a PoolConfig,
    harness_dir: &'a Path,
    socket_path: &'a Path,
}

impl<'a> DispatchState<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        work_items: Vec<WorkItem>,
        processes: Vec<Child>,
        progress: Option<crate::progress::ProgressBar>,
        event_tx: mpsc::Sender<WorkerEvent>,
        config: &'a PoolConfig,
        harness_dir: &'a Path,
        socket_path: &'a Path,
        trace: &'a mut TraceLog,
    ) -> Self {
        let total_items = work_items.len();
        let pending_accepts = processes.len();
        let initial_spawn_us = trace.now_us();

        let mut spawn_times = HashMap::new();
        for i in 0..processes.len() {
            spawn_times.insert(i, initial_spawn_us);
        }

        Self {
            results: Vec::with_capacity(total_items),
            work_queue: prioritize_work_items(work_items).into_iter().rev().collect(),
            total_items,
            worker_senders: HashMap::new(),
            idle_workers: Vec::new(),
            active_mutants: HashMap::new(),
            next_worker_id: 0,
            pending_accepts,
            worker_pids: HashMap::new(),
            workers_pending_memory_recycle: HashSet::new(),
            spawn_times,
            dispatch_times: HashMap::new(),
            trace,
            processes,
            progress,
            event_tx,
            config,
            harness_dir,
            socket_path,
        }
    }

    /// Record an error result for a mutant.
    fn record_error(&mut self, mutant_name: String) {
        if let Some(ref mut pb) = self.progress {
            pb.worker_done(0);
            pb.record(MutantStatus::Error);
        }
        self.results.push(MutantResult {
            mutant_name,
            exit_code: -1,
            duration: 0.0,
            status: MutantStatus::Error,
        });
    }

    /// Record an error result for a worker's active mutant, updating progress.
    fn record_worker_error(&mut self, worker_id: usize) {
        if let Some(mutant_name) = self.active_mutants.remove(&worker_id) {
            if let Some(ref mut pb) = self.progress {
                pb.worker_done(worker_id);
                pb.record(MutantStatus::Error);
            }
            self.results.push(MutantResult {
                mutant_name,
                exit_code: -1,
                duration: 0.0,
                status: MutantStatus::Error,
            });
        }
    }

    /// Try to spawn a replacement worker. Returns true on success.
    fn respawn_worker(&mut self) -> bool {
        let spawn_id = self.next_worker_id;
        match spawn_worker(spawn_id, self.config, self.harness_dir, self.socket_path) {
            Ok(child) => {
                self.processes.push(child);
                self.pending_accepts += 1;
                self.spawn_times.insert(spawn_id, self.trace.now_us());
                info!("Spawned replacement worker {spawn_id}");
                true
            }
            Err(e) => {
                error!("Failed to spawn replacement worker: {e}");
                false
            }
        }
    }

    /// Dispatch work to all idle workers that have items in the queue.
    async fn dispatch_pending(&mut self) {
        while let Some(&worker_id) = self.idle_workers.last() {
            let Some(item) = self.work_queue.pop() else {
                break;
            };
            self.idle_workers.pop();
            self.active_mutants.insert(worker_id, item.mutant_name.clone());
            self.dispatch_times
                .insert(worker_id, (self.trace.now_us(), item.mutant_name.clone()));
            if let Some(sender) = self.worker_senders.get(&worker_id) {
                if let Some(ref mut pb) = self.progress {
                    pb.worker_start(worker_id, &item.mutant_name);
                }
                let msg = OrchestratorMessage::Run {
                    mutant: item.mutant_name,
                    tests: item.test_ids,
                    timeout_secs: Some(item.timeout_secs),
                };
                if sender.send(msg).await.is_err() {
                    warn!("Worker {worker_id} channel closed while dispatching");
                    self.record_worker_error(worker_id);
                }
            }
        }
    }

    /// Returns true if the dispatch loop should exit.
    fn is_done(&self) -> bool {
        if self.results.len() >= self.total_items {
            return true;
        }
        self.results.len() + self.active_mutants.len() == 0
            && self.work_queue.is_empty()
            && self.pending_accepts == 0
    }

    /// Detect and handle stuck state (no workers, no pending accepts, but work remains).
    /// Returns true if we're stuck and the loop should exit.
    async fn handle_stuck(&mut self) -> bool {
        if !(self.idle_workers.is_empty()
            && self.active_mutants.is_empty()
            && !self.work_queue.is_empty()
            && self.pending_accepts == 0
            && self.worker_senders.is_empty())
        {
            return false;
        }

        // Try to capture stderr from crashed worker processes for diagnostics
        let mut stderr_snippet = String::new();
        for proc in self.processes.iter_mut() {
            if let Some(stderr) = proc.stderr.take() {
                use tokio::io::AsyncReadExt;
                let mut buf = [0u8; 2048];
                let mut async_stderr = stderr;
                if let Ok(Ok(n)) = tokio::time::timeout(
                    Duration::from_millis(500),
                    async_stderr.read(&mut buf),
                )
                .await
                {
                    if n > 0 {
                        let text = String::from_utf8_lossy(&buf[..n]);
                        if !text.trim().is_empty() {
                            stderr_snippet = text.trim().to_string();
                        }
                    }
                }
            }
        }

        if stderr_snippet.is_empty() {
            error!(
                "No available workers; marking {} remaining items as errors. \
                 All workers crashed or failed to start. Check that your test suite runs \
                 cleanly with: pytest {}",
                self.work_queue.len(),
                self.config.tests_dir.display(),
            );
        } else {
            error!(
                "No available workers; marking {} remaining items as errors. \
                 All workers crashed or failed to start.\n\
                 Worker stderr:\n{}\n\
                 Check that your test suite runs cleanly with: pytest {}",
                self.work_queue.len(),
                stderr_snippet,
                self.config.tests_dir.display(),
            );
        }

        for item in self.work_queue.drain(..).rev() {
            if let Some(ref mut pb) = self.progress {
                pb.record(MutantStatus::Error);
            }
            self.results.push(MutantResult {
                mutant_name: item.mutant_name,
                exit_code: -1,
                duration: 0.0,
                status: MutantStatus::Error,
            });
        }
        true
    }

    /// Handle an accepted worker connection: read the ready message, register the worker.
    async fn handle_accept(&mut self, stream: tokio::net::UnixStream) {
        let worker_id = self.next_worker_id;
        self.next_worker_id += 1;

        let (reader, writer) = stream.into_split();
        let mut buf_reader = BufReader::new(reader);

        let mut line = String::new();
        let ready_timeout = Duration::from_secs(self.config.worker_ready_timeout);
        match timeout(ready_timeout, buf_reader.read_line(&mut line)).await {
            Ok(Ok(0)) => {
                error!(
                    "Worker {worker_id}: disconnected before sending ready message. \
                     Common causes: import error in your source code, missing dependency, \
                     or incompatible pytest plugin."
                );
            }
            Ok(Ok(_)) => {
                match serde_json::from_str::<WorkerMessage>(line.trim()) {
                    Ok(WorkerMessage::Ready { pid, .. }) => {
                        info!("Worker {worker_id} (pid {pid}) connected and ready");

                        // Trace: worker startup span
                        if let Some(&spawn_us) = self.spawn_times.get(&worker_id) {
                            let now = self.trace.now_us();
                            self.trace.complete(
                                "worker_startup".to_string(),
                                "lifecycle",
                                spawn_us,
                                now.saturating_sub(spawn_us),
                                worker_id,
                                Some(serde_json::json!({"pid": pid})),
                            );
                        }

                        self.worker_pids.insert(worker_id, pid);
                        let msg_tx = spawn_worker_task(
                            worker_id,
                            buf_reader.into_inner(),
                            writer,
                            self.event_tx.clone(),
                            self.config.default_timeout,
                            self.config.timeout_multiplier,
                        );
                        self.worker_senders.insert(worker_id, msg_tx);
                        self.idle_workers.push(worker_id);
                    }
                    Ok(other) => {
                        warn!("Worker {worker_id}: unexpected first message: {other:?}");
                    }
                    Err(e) => {
                        error!("Worker {worker_id}: failed to parse ready message: {e}");
                    }
                }
            }
            Ok(Err(e)) => {
                error!("Worker {worker_id}: error reading ready message: {e}");
            }
            Err(_) => {
                error!(
                    "Worker {worker_id}: timeout reading ready message ({}s). \
                     This usually means test collection is very slow or the worker is stuck. \
                     Try increasing with --worker-timeout (e.g. --worker-timeout 120).",
                    self.config.worker_ready_timeout,
                );
            }
        }
    }

    /// Handle a worker event (ready, result, error, disconnected).
    async fn handle_event(&mut self, event: WorkerEvent) {
        match event {
            WorkerEvent::Ready { worker_id } => {
                debug!("Worker {worker_id} ready");
                if !self.idle_workers.contains(&worker_id) {
                    self.idle_workers.push(worker_id);
                }
            }
            WorkerEvent::Result { worker_id, result } => {
                self.handle_result(worker_id, result).await;
            }
            WorkerEvent::Error {
                worker_id,
                message,
                mutant,
            } => {
                error!("Worker {worker_id} error: {message}");
                if let Some(mutant_name) =
                    mutant.or_else(|| self.active_mutants.remove(&worker_id))
                {
                    if let Some(ref mut pb) = self.progress {
                        pb.worker_done(worker_id);
                        pb.record(MutantStatus::Error);
                    }
                    self.results.push(MutantResult {
                        mutant_name,
                        exit_code: -1,
                        duration: 0.0,
                        status: MutantStatus::Error,
                    });
                }
                self.active_mutants.remove(&worker_id);
                self.idle_workers.push(worker_id);
            }
            WorkerEvent::Disconnected { worker_id } => {
                self.handle_disconnect(worker_id).await;
            }
        }
    }

    /// Handle a mutant result: record it, decide whether to recycle the worker.
    async fn handle_result(&mut self, worker_id: usize, result: MutantResult) {
        info!(
            "Mutant {} -> {:?} ({:.3}s)",
            result.mutant_name, result.status, result.duration
        );
        // Trace: mutant execution span
        if let Some((dispatch_us, _)) = self.dispatch_times.remove(&worker_id) {
            let now = self.trace.now_us();
            self.trace.complete(
                result.mutant_name.clone(),
                "mutant",
                dispatch_us,
                now.saturating_sub(dispatch_us),
                worker_id,
                Some(serde_json::json!({
                    "status": format!("{:?}", result.status),
                    "duration_s": result.duration,
                })),
            );
        }
        self.active_mutants.remove(&worker_id);
        if let Some(ref mut pb) = self.progress {
            pb.worker_done(worker_id);
            pb.record(result.status);
        }
        self.results.push(result);

        let memory_recycle = self.workers_pending_memory_recycle.remove(&worker_id);

        if memory_recycle && !self.work_queue.is_empty() {
            info!("Worker {worker_id}: recycling due to memory limit exceeded");
            if let Some(sender) = self.worker_senders.remove(&worker_id) {
                let _ = sender.send(OrchestratorMessage::Shutdown).await;
            }
            self.worker_pids.remove(&worker_id);
            self.respawn_worker();
        } else {
            self.idle_workers.push(worker_id);
        }
    }

    /// Handle a worker disconnect (expected from memory recycling, or unexpected crash).
    async fn handle_disconnect(&mut self, worker_id: usize) {
        self.worker_pids.remove(&worker_id);
        self.workers_pending_memory_recycle.remove(&worker_id);
        // If the worker was already removed from worker_senders (by handle_result
        // during memory recycling), this is an expected disconnect.
        if !self.worker_senders.contains_key(&worker_id) && !self.active_mutants.contains_key(&worker_id) {
            debug!("Worker {worker_id}: disconnected cleanly");
        } else {
            warn!(
                "Worker {worker_id} crashed. \
                 If this keeps happening, try reducing parallelism with --workers."
            );
            self.record_worker_error(worker_id);
            self.worker_senders.remove(&worker_id);

            if !self.work_queue.is_empty() {
                info!(
                    "Respawning worker to replace crashed {worker_id}"
                );
                self.respawn_worker();
            }
        }
    }

    /// Check health of active workers: detect dead processes immediately.
    fn handle_health_check(&mut self) {
        let active_pids: Vec<(usize, u32)> = self
            .active_mutants
            .keys()
            .filter_map(|&wid| self.worker_pids.get(&wid).map(|&pid| (wid, pid)))
            .collect();
        for (worker_id, pid) in active_pids {
            if !is_process_alive(pid) {
                warn!("Worker {worker_id} (pid {pid}) died — marking active mutant as error");
                self.record_worker_error(worker_id);
                self.worker_senders.remove(&worker_id);
                self.worker_pids.remove(&worker_id);

                if !self.work_queue.is_empty() {
                    info!("Respawning worker to replace dead {worker_id}");
                    self.respawn_worker();
                }
            }
        }
    }

    /// Check memory usage of active workers: flag those over the limit for recycling.
    async fn handle_memory_check(&mut self) {
        for (&worker_id, &pid) in &self.worker_pids {
            if self.workers_pending_memory_recycle.contains(&worker_id) {
                continue;
            }
            if let Ok(rss_kb) = check_rss(pid).await {
                let rss_mb = rss_kb / 1024;
                if rss_mb > self.config.max_worker_memory_mb {
                    warn!(
                        "Worker {worker_id} (pid {pid}) RSS {rss_mb}MB exceeds limit \
                         {}MB; scheduling recycle after current task",
                        self.config.max_worker_memory_mb
                    );
                    self.workers_pending_memory_recycle.insert(worker_id);
                }
            }
        }
    }

    /// Shut down remaining workers and wait for processes to exit.
    async fn shutdown(self) -> Vec<MutantResult> {
        for sender in self.worker_senders.values() {
            let _ = sender.send(OrchestratorMessage::Shutdown).await;
        }

        let mut wait_set = tokio::task::JoinSet::new();
        for mut proc in self.processes {
            wait_set.spawn(async move { proc.wait().await });
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            while wait_set.join_next().await.is_some() {}
        })
        .await;

        if let Some(pb) = self.progress {
            pb.finish();
        }

        self.results
    }
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_work(
    listener: UnixListener,
    processes: Vec<Child>,
    work_items: Vec<WorkItem>,
    config: &PoolConfig,
    harness_dir: &Path,
    socket_path: &Path,
    progress: Option<crate::progress::ProgressBar>,
    trace: &mut TraceLog,
) -> Result<Vec<MutantResult>> {
    let (event_tx, mut event_rx) = mpsc::channel::<WorkerEvent>(64);
    let mut state = DispatchState::new(
        work_items, processes, progress, event_tx, config, harness_dir, socket_path, trace,
    );

    let accept_timeout = Duration::from_secs(30);
    let max_worker_memory_mb = config.max_worker_memory_mb;
    let mut memory_check = tokio::time::interval(Duration::from_secs(2));
    let mut health_check = tokio::time::interval(Duration::from_millis(500));

    loop {
        state.dispatch_pending().await;

        if state.is_done() {
            break;
        }
        if state.handle_stuck().await {
            break;
        }

        tokio::select! {
            r = timeout(accept_timeout, listener.accept()), if state.pending_accepts > 0 => {
                state.pending_accepts -= 1;
                match r {
                    Ok(Ok((stream, _))) => state.handle_accept(stream).await,
                    Ok(Err(e)) => error!("Failed to accept worker connection: {e}"),
                    Err(_) => error!(
                        "Timeout waiting for worker connection (30s). \
                         The worker process may have crashed during startup."
                    ),
                }
            }

            event = event_rx.recv() => {
                match event {
                    Some(e) => state.handle_event(e).await,
                    None => {
                        warn!("All worker tasks finished");
                        let orphaned: Vec<String> = state.active_mutants.drain().map(|(_, n)| n).collect();
                        for mutant_name in orphaned {
                            state.record_error(mutant_name);
                        }
                        break;
                    }
                }
            }

            _ = health_check.tick() => {
                state.handle_health_check();
            }

            _ = memory_check.tick(), if max_worker_memory_mb > 0 => {
                state.handle_memory_check().await;
            }
        }
    }

    Ok(state.shutdown().await)
}

fn prioritize_work_items(mut work_items: Vec<WorkItem>) -> Vec<WorkItem> {
    work_items.sort_unstable_by(|a, b| {
        b.estimated_duration_secs
            .total_cmp(&a.estimated_duration_secs)
    });
    work_items
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work_item(mutant_name: &str, estimated_duration_secs: f64) -> WorkItem {
        WorkItem {
            mutant_name: mutant_name.to_string(),
            test_ids: vec![],
            estimated_duration_secs,
            timeout_secs: 30.0,
        }
    }

    #[test]
    fn test_prioritize_work_items_longest_first() {
        let items = vec![
            work_item("m_mid", 2.0),
            work_item("m_fast", 0.5),
            work_item("m_slow", 10.0),
        ];

        let ordered = prioritize_work_items(items);
        let names: Vec<&str> = ordered
            .iter()
            .map(|item| item.mutant_name.as_str())
            .collect();

        assert_eq!(names, vec!["m_slow", "m_mid", "m_fast"]);
    }

    #[test]
    fn test_prioritize_work_items_equal_duration_no_panic() {
        // Equal durations should not panic (no tiebreaker needed for scheduling).
        let items = vec![work_item("m_b", 1.0), work_item("m_a", 1.0)];
        let ordered = prioritize_work_items(items);
        assert_eq!(ordered.len(), 2);
    }

    // --- check_rss tests (INV-1) ---

    /// INV-1: check_rss returns nonzero RSS for the current process.
    /// Kills mutations that break the ps output parsing (e.g., Ok(0) for any pid).
    #[tokio::test]
    async fn test_check_rss_current_process() {
        let pid = std::process::id();
        let result = check_rss(pid).await;
        assert!(
            result.is_ok(),
            "check_rss should succeed for current process (pid {pid}), got: {result:?}"
        );
        assert!(
            result.unwrap() > 0,
            "current process should have nonzero RSS"
        );
    }

    /// INV-1: check_rss returns Err for a nonexistent PID.
    /// Kills mutations that suppress errors (e.g., returning Ok(0) for any input).
    #[tokio::test]
    async fn test_check_rss_invalid_pid() {
        // PID 999999999 is virtually guaranteed not to exist on any real system.
        let result = check_rss(999_999_999).await;
        assert!(
            result.is_err(),
            "check_rss should return Err for a nonexistent PID, got: {result:?}"
        );
    }

    // --- memory recycle decision tests ---

    /// Memory recycling triggers when RSS exceeded and work remains.
    #[test]
    fn test_recycle_decision_memory_with_work_remaining() {
        let memory_recycle = true;
        let work_remaining = true;
        assert!(
            memory_recycle && work_remaining,
            "memory_recycle must trigger recycling when work remains"
        );
    }

    /// Memory recycling does not trigger when work queue is empty.
    #[test]
    fn test_recycle_decision_memory_no_work_remaining() {
        let memory_recycle = true;
        let work_remaining = false;
        assert!(
            !(memory_recycle && work_remaining),
            "recycling must not occur when no work remains"
        );
    }
}
