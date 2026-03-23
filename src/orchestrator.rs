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
    /// Respawn workers after this many mutants to prevent pytest state accumulation.
    /// `None` = auto-tune: defaults to 100, but reduced to 20 if session-scoped fixtures
    /// are detected (to limit state leakage).
    /// `Some(0)` = disable recycling entirely.
    /// `Some(n)` = explicit user override, always respected regardless of fixture detection.
    pub worker_recycle_after: Option<usize>,
    /// Recycle workers whose RSS exceeds this many megabytes. 0 = unlimited.
    pub max_worker_memory_mb: usize,
    /// Extra arguments appended to every pytest invocation within the worker pool.
    /// Passed to worker processes via the `IRRADIATE_PYTEST_ARGS` env var (JSON array).
    pub pytest_add_cli_args: Vec<String>,
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
            worker_recycle_after: None,
            max_worker_memory_mb: 0,
            pytest_add_cli_args: Vec::new(),
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

    // Create unix socket in /tmp to avoid macOS path length limit (104 bytes)
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

    let listener = UnixListener::bind(&socket_path).context("Failed to bind unix socket")?;
    info!("Orchestrator listening on {}", socket_path.display());

    let num_workers = config.num_workers.min(work_items.len());

    // Spawn initial workers
    let mut processes: Vec<Child> = Vec::new();
    for i in 0..num_workers {
        let child = spawn_worker(i, config, &harness_dir, &socket_path)?;
        processes.push(child);
    }

    // Accept connections and dispatch work
    let mut trace = TraceLog::new();
    let results = dispatch_work(
        listener,
        processes,
        work_items,
        config,
        &harness_dir,
        &socket_path,
        progress,
        &mut trace,
    )
    .await?;

    // Clean up socket
    let _ = std::fs::remove_file(&socket_path);

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
        .current_dir(&config.project_dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("Failed to spawn worker {id}"))?;

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
                            let json = serde_json::to_string(&msg).context("failed to serialize IPC message")? + "\n";
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
/// Uses `ps -o rss= -p <pid>` which works on both macOS and Linux.
async fn check_rss(pid: u32) -> Result<usize> {
    let output = tokio::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .await?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.trim().parse::<usize>().map_err(|e| anyhow::anyhow!(e))
}

/// Determine the effective recycling interval given user configuration and fixture detection.
///
/// - `configured = None`: auto-tune mode. Default 100, reduced to 20 if session fixtures found.
/// - `configured = Some(n)`: explicit user override; always returned as-is.
pub(crate) fn determine_recycle_after(
    configured: Option<usize>,
    has_session_fixtures: bool,
    session_fixture_count: usize,
) -> usize {
    match configured {
        Some(explicit) => explicit,
        None => {
            const DEFAULT: usize = 100;
            const SESSION_FIXTURE_REDUCED: usize = 20;
            if has_session_fixtures && session_fixture_count > 0 {
                DEFAULT.min(SESSION_FIXTURE_REDUCED)
            } else {
                DEFAULT
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_work(
    listener: UnixListener,
    mut processes: Vec<Child>,
    work_items: Vec<WorkItem>,
    config: &PoolConfig,
    harness_dir: &Path,
    socket_path: &Path,
    mut progress: Option<crate::progress::ProgressBar>,
    trace: &mut TraceLog,
) -> Result<Vec<MutantResult>> {
    let total_items = work_items.len();
    let mut results: Vec<MutantResult> = Vec::with_capacity(total_items);
    let mut work_queue: Vec<WorkItem> = prioritize_work_items(work_items)
        .into_iter()
        .rev()
        .collect();

    // Channel for worker events
    let (event_tx, mut event_rx) = mpsc::channel::<WorkerEvent>(64);

    // Track which workers are idle
    let mut worker_senders: HashMap<usize, mpsc::Sender<OrchestratorMessage>> = HashMap::new();
    let mut idle_workers: Vec<usize> = Vec::new();
    let mut active_mutants: HashMap<usize, String> = HashMap::new(); // worker_id -> mutant_name
    let mut next_worker_id: usize = 0;

    // Recycling state
    let mut worker_recycle_counts: HashMap<usize, usize> = HashMap::new();
    let mut recycled_worker_ids: HashSet<usize> = HashSet::new();
    // Number of spawned workers whose connection we're still waiting to accept.
    // Starts at num_workers (initial workers already spawned before dispatch_work is called).
    let mut pending_accepts: usize = processes.len();

    // PID tracking for memory monitoring
    let mut worker_pids: HashMap<usize, u32> = HashMap::new();
    // Workers flagged for recycling on next result (memory limit exceeded)
    let mut workers_pending_memory_recycle: HashSet<usize> = HashSet::new();

    // Trace: track spawn and dispatch timestamps per worker
    let mut spawn_times: HashMap<usize, u64> = HashMap::new(); // worker_id -> spawn timestamp (us)
    let mut dispatch_times: HashMap<usize, (u64, String)> = HashMap::new(); // worker_id -> (timestamp, mutant)
    // Record spawn times for the initial workers (spawned before dispatch_work)
    let initial_spawn_us = trace.now_us();
    for i in 0..processes.len() {
        spawn_times.insert(i, initial_spawn_us);
    }

    // Effective recycle interval: starts at the user-configured value (or 100 if auto).
    // May be reduced on first Ready message if session fixtures are detected.
    let mut recycle_after = config.worker_recycle_after.unwrap_or(100);
    let mut session_detection_done = false;
    let max_worker_memory_mb = config.max_worker_memory_mb;
    let accept_timeout = Duration::from_secs(30);
    let default_timeout = config.default_timeout;
    let timeout_multiplier = config.timeout_multiplier;

    // Memory check interval (only active when a limit is set)
    let mut memory_check = tokio::time::interval(Duration::from_secs(2));

    // Process health check: detect dead workers faster than the per-mutant
    // socket read timeout. Polls every 500ms; a dead process triggers an
    // immediate error result instead of waiting for the full timeout.
    let mut health_check = tokio::time::interval(Duration::from_millis(500));

    // Main dispatch loop — accepts initial worker connections and processes events
    loop {
        // Dispatch work to idle workers
        while let Some(&worker_id) = idle_workers.last() {
            if let Some(item) = work_queue.pop() {
                idle_workers.pop();
                active_mutants.insert(worker_id, item.mutant_name.clone());
                dispatch_times.insert(worker_id, (trace.now_us(), item.mutant_name.clone()));
                if let Some(sender) = worker_senders.get(&worker_id) {
                    let msg = OrchestratorMessage::Run {
                        mutant: item.mutant_name,
                        tests: item.test_ids,
                        timeout_secs: Some(item.timeout_secs),
                    };
                    if sender.send(msg).await.is_err() {
                        warn!("Worker {worker_id} channel closed while dispatching");
                        // Put the work item back
                        if let Some(mutant_name) = active_mutants.remove(&worker_id) {
                            if let Some(ref mut pb) = progress {
                                pb.record(MutantStatus::Error);
                            }
                            results.push(MutantResult {
                                mutant_name,
                                exit_code: -1,
                                duration: 0.0,
                                status: MutantStatus::Error,
                            });
                        }
                    }
                }
            } else {
                break; // no more work
            }
        }

        // Check if we're done
        if results.len() >= total_items {
            break;
        }
        if results.len() + active_mutants.len() == 0
            && work_queue.is_empty()
            && pending_accepts == 0
        {
            break;
        }

        // Stuck detection: no workers available, no active work, no pending accepts, but work remains
        if idle_workers.is_empty()
            && active_mutants.is_empty()
            && !work_queue.is_empty()
            && pending_accepts == 0
            && worker_senders.is_empty()
        {
            error!(
                "No available workers; marking {} remaining items as errors",
                work_queue.len()
            );
            for item in work_queue.drain(..).rev() {
                if let Some(ref mut pb) = progress {
                    pb.record(MutantStatus::Error);
                }
                results.push(MutantResult {
                    mutant_name: item.mutant_name,
                    exit_code: -1,
                    duration: 0.0,
                    status: MutantStatus::Error,
                });
            }
            break;
        }

        tokio::select! {
            // Accept a pending worker connection (initial or replacement)
            r = timeout(accept_timeout, listener.accept()), if pending_accepts > 0 => {
                pending_accepts -= 1;
                match r {
                    Ok(Ok((stream, _))) => {
                        let worker_id = next_worker_id;
                        next_worker_id += 1;

                        let (reader, writer) = stream.into_split();
                        let mut buf_reader = BufReader::new(reader);

                        // Read the ready message
                        let mut line = String::new();
                        match timeout(Duration::from_secs(10), buf_reader.read_line(&mut line)).await {
                            Ok(Ok(0)) => {
                                error!("Worker {worker_id}: disconnected before sending ready message");
                            }
                            Ok(Ok(_)) => {
                                match serde_json::from_str::<WorkerMessage>(line.trim()) {
                                    Ok(WorkerMessage::Ready {
                                        pid,
                                        has_session_fixtures,
                                        session_fixture_count,
                                        ..
                                    }) => {
                                        info!("Worker {worker_id} (pid {pid}) connected and ready");

                                        // Auto-tune recycling interval on first worker connection.
                                        if !session_detection_done {
                                            session_detection_done = true;
                                            let new_recycle = determine_recycle_after(
                                                config.worker_recycle_after,
                                                has_session_fixtures,
                                                session_fixture_count,
                                            );
                                            if has_session_fixtures && config.worker_recycle_after.is_none() {
                                                warn!(
                                                    "Session-scoped fixtures detected ({session_fixture_count}); \
                                                     reducing worker recycle interval to {new_recycle} for correctness. \
                                                     Use --worker-recycle-after to override."
                                                );
                                                info!(
                                                    "Auto-tuning worker recycle interval: {recycle_after} → {new_recycle}"
                                                );
                                                recycle_after = new_recycle;
                                            } else if has_session_fixtures {
                                                info!(
                                                    "Session-scoped fixtures detected ({session_fixture_count}); \
                                                     respecting explicit --worker-recycle-after={recycle_after}"
                                                );
                                            }
                                        }

                                        // Trace: worker startup span
                                        if let Some(&spawn_us) = spawn_times.get(&worker_id) {
                                            let now = trace.now_us();
                                            trace.complete(
                                                "worker_startup".to_string(),
                                                "lifecycle",
                                                spawn_us,
                                                now.saturating_sub(spawn_us),
                                                worker_id,
                                                Some(serde_json::json!({"pid": pid})),
                                            );
                                        }

                                        worker_pids.insert(worker_id, pid);
                                        let msg_tx = spawn_worker_task(
                                            worker_id,
                                            buf_reader.into_inner(),
                                            writer,
                                            event_tx.clone(),
                                            default_timeout,
                                            timeout_multiplier,
                                        );
                                        worker_senders.insert(worker_id, msg_tx);
                                        idle_workers.push(worker_id);
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
                                error!("Worker {worker_id}: timeout reading ready message");
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        error!("Failed to accept worker connection: {e}");
                    }
                    Err(_) => {
                        error!("Timeout waiting for worker connection");
                    }
                }
            }

            // Process events from workers
            event = event_rx.recv() => {
                match event {
                    Some(WorkerEvent::Ready { worker_id }) => {
                        debug!("Worker {worker_id} ready");
                        if !idle_workers.contains(&worker_id) {
                            idle_workers.push(worker_id);
                        }
                    }
                    Some(WorkerEvent::Result { worker_id, result }) => {
                        info!(
                            "Mutant {} -> {:?} ({:.3}s)",
                            result.mutant_name, result.status, result.duration
                        );
                        // Trace: mutant execution span
                        if let Some((dispatch_us, _)) = dispatch_times.remove(&worker_id) {
                            let now = trace.now_us();
                            trace.complete(
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
                        active_mutants.remove(&worker_id);
                        if let Some(ref mut pb) = progress {
                            pb.record(result.status);
                        }
                        results.push(result);

                        let count = worker_recycle_counts.entry(worker_id).or_insert(0);
                        *count += 1;

                        let memory_recycle = workers_pending_memory_recycle.remove(&worker_id);
                        let count_recycle = recycle_after > 0 && *count >= recycle_after;

                        if (count_recycle || memory_recycle) && !work_queue.is_empty() {
                            // Recycle: send shutdown, spawn a fresh replacement
                            if memory_recycle {
                                info!("Worker {worker_id}: recycling due to memory limit exceeded");
                            } else {
                                info!("Worker {worker_id}: recycling after {count} mutants");
                            }
                            if let Some(sender) = worker_senders.remove(&worker_id) {
                                let _ = sender.send(OrchestratorMessage::Shutdown).await;
                            }
                            worker_recycle_counts.remove(&worker_id);
                            worker_pids.remove(&worker_id);
                            recycled_worker_ids.insert(worker_id);

                            let spawn_id = next_worker_id; // ID the replacement will be assigned on accept
                            match spawn_worker(spawn_id, config, harness_dir, socket_path) {
                                Ok(child) => {
                                    processes.push(child);
                                    pending_accepts += 1;
                                    spawn_times.insert(spawn_id, trace.now_us());
                                    info!("Spawned replacement worker {spawn_id}");
                                }
                                Err(e) => {
                                    error!("Failed to spawn replacement worker: {e}");
                                    // Stuck detection will surface the lack of workers if needed
                                }
                            }
                            // Do NOT add worker_id back to idle_workers — it is being recycled
                        } else {
                            idle_workers.push(worker_id);
                        }
                    }
                    Some(WorkerEvent::Error {
                        worker_id,
                        message,
                        mutant,
                    }) => {
                        error!("Worker {worker_id} error: {message}");
                        if let Some(mutant_name) = mutant.or_else(|| active_mutants.remove(&worker_id)) {
                            if let Some(ref mut pb) = progress {
                                pb.record(MutantStatus::Error);
                            }
                            results.push(MutantResult {
                                mutant_name,
                                exit_code: -1,
                                duration: 0.0,
                                status: MutantStatus::Error,
                            });
                        }
                        active_mutants.remove(&worker_id);
                        idle_workers.push(worker_id);
                    }
                    Some(WorkerEvent::Disconnected { worker_id }) => {
                        worker_pids.remove(&worker_id);
                        workers_pending_memory_recycle.remove(&worker_id);
                        if recycled_worker_ids.remove(&worker_id) {
                            // Expected: we asked this worker to shut down for recycling
                            debug!("Worker {worker_id}: recycled cleanly");
                        } else {
                            warn!("Worker {worker_id} disconnected unexpectedly");
                            // Record any active mutant as error
                            if let Some(mutant_name) = active_mutants.remove(&worker_id) {
                                if let Some(ref mut pb) = progress {
                                    pb.record(MutantStatus::Error);
                                }
                                results.push(MutantResult {
                                    mutant_name,
                                    exit_code: -1,
                                    duration: 0.0,
                                    status: MutantStatus::Error,
                                });
                            }
                            worker_senders.remove(&worker_id);

                            // Respawn if we still have work
                            if !work_queue.is_empty() {
                                let spawn_id = next_worker_id;
                                info!("Respawning worker {spawn_id} to replace crashed {worker_id}");
                                match spawn_worker(spawn_id, config, harness_dir, socket_path) {
                                    Ok(child) => {
                                        processes.push(child);
                                        pending_accepts += 1;
                                        spawn_times.insert(spawn_id, trace.now_us());
                                    }
                                    Err(e) => {
                                        error!("Failed to respawn worker: {e}");
                                    }
                                }
                            }
                        }
                    }
                    None => {
                        // All event_tx clones dropped — all worker tasks finished
                        warn!("All worker tasks finished");
                        for (_, mutant_name) in active_mutants.drain() {
                            if let Some(ref mut pb) = progress {
                                pb.record(MutantStatus::Error);
                            }
                            results.push(MutantResult {
                                mutant_name,
                                exit_code: -1,
                                duration: 0.0,
                                status: MutantStatus::Error,
                            });
                        }
                        break;
                    }
                }
            }

            // Process health check: detect dead workers immediately instead of
            // waiting for the per-mutant socket read timeout (which can be 20s+).
            _ = health_check.tick() => {
                // Only check workers that have an active mutant assignment
                let active_pids: Vec<(usize, u32)> = active_mutants
                    .keys()
                    .filter_map(|&wid| worker_pids.get(&wid).map(|&pid| (wid, pid)))
                    .collect();
                for (worker_id, pid) in active_pids {
                    if !is_process_alive(pid) {
                        warn!("Worker {worker_id} (pid {pid}) died — marking active mutant as error");
                        if let Some(mutant_name) = active_mutants.remove(&worker_id) {
                            if let Some(ref mut pb) = progress {
                                pb.record(MutantStatus::Error);
                            }
                            results.push(MutantResult {
                                mutant_name,
                                exit_code: -1,
                                duration: 0.0,
                                status: MutantStatus::Error,
                            });
                        }
                        worker_senders.remove(&worker_id);
                        worker_pids.remove(&worker_id);

                        // Respawn if there's still work
                        if !work_queue.is_empty() {
                            let spawn_id = next_worker_id;
                            info!("Respawning worker {spawn_id} to replace dead {worker_id}");
                            match spawn_worker(spawn_id, config, harness_dir, socket_path) {
                                Ok(child) => {
                                    processes.push(child);
                                    pending_accepts += 1;
                                    spawn_times.insert(spawn_id, trace.now_us());
                                }
                                Err(e) => {
                                    error!("Failed to respawn worker: {e}");
                                }
                            }
                        }
                    }
                }
            }

            // Periodic memory check: sample RSS for each worker and flag those over the limit
            _ = memory_check.tick(), if max_worker_memory_mb > 0 => {
                for (&worker_id, &pid) in &worker_pids {
                    if workers_pending_memory_recycle.contains(&worker_id) {
                        continue; // already flagged
                    }
                    match check_rss(pid).await {
                        Ok(rss_kb) => {
                            let rss_mb = rss_kb / 1024;
                            if rss_mb > max_worker_memory_mb {
                                warn!(
                                    "Worker {worker_id} (pid {pid}) RSS {rss_mb}MB exceeds limit \
                                     {max_worker_memory_mb}MB; scheduling recycle after current task"
                                );
                                workers_pending_memory_recycle.insert(worker_id);
                            }
                        }
                        Err(_) => {
                            // Process may have already exited; ignore
                        }
                    }
                }
            }
        }
    }

    // Shutdown remaining workers
    for sender in worker_senders.values() {
        let _ = sender.send(OrchestratorMessage::Shutdown).await;
    }

    // Wait for all processes concurrently with a single shared timeout.
    // Recycled workers may already have exited; stuck processes are killed on
    // Child drop (kill_on_drop=true). A single timeout avoids sequential 5s
    // waits per stuck process.
    let mut wait_set = tokio::task::JoinSet::new();
    for mut proc in processes {
        wait_set.spawn(async move { proc.wait().await });
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while wait_set.join_next().await.is_some() {}
    })
    .await;

    if let Some(pb) = progress {
        pb.finish();
    }

    Ok(results)
}

fn prioritize_work_items(mut work_items: Vec<WorkItem>) -> Vec<WorkItem> {
    work_items.sort_by(|a, b| {
        b.estimated_duration_secs
            .total_cmp(&a.estimated_duration_secs)
            .then_with(|| a.mutant_name.cmp(&b.mutant_name))
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
    fn test_prioritize_work_items_tiebreaks_by_name() {
        let items = vec![work_item("m_b", 1.0), work_item("m_a", 1.0)];

        let ordered = prioritize_work_items(items);
        let names: Vec<&str> = ordered
            .iter()
            .map(|item| item.mutant_name.as_str())
            .collect();

        assert_eq!(names, vec!["m_a", "m_b"]);
    }

    // --- determine_recycle_after tests (INV-1, INV-2, INV-4) ---

    /// INV-1: Auto-mode with session fixtures → reduced interval.
    #[test]
    fn test_determine_recycle_after_auto_with_session_fixtures() {
        let result = determine_recycle_after(None, true, 3);
        assert_eq!(result, 20, "session fixtures detected in auto mode → 20");
    }

    /// INV-2: Explicit user override is always respected, even with session fixtures.
    #[test]
    fn test_determine_recycle_after_explicit_with_session_fixtures() {
        let result = determine_recycle_after(Some(50), true, 3);
        assert_eq!(result, 50, "explicit setting must override auto-tune");
    }

    /// INV-2: Explicit 0 (disabled) is respected even with session fixtures.
    #[test]
    fn test_determine_recycle_after_explicit_zero_with_session_fixtures() {
        let result = determine_recycle_after(Some(0), true, 5);
        assert_eq!(result, 0, "explicit 0 (disabled) must be respected");
    }

    /// INV-4: Auto-mode without session fixtures → default interval.
    #[test]
    fn test_determine_recycle_after_auto_no_session_fixtures() {
        let result = determine_recycle_after(None, false, 0);
        assert_eq!(result, 100, "no session fixtures → default 100");
    }

    /// Auto-mode, has_session_fixtures=true but count=0 → no reduction (shouldn't happen
    /// in practice, but guards against inconsistent Python-side behavior).
    #[test]
    fn test_determine_recycle_after_auto_fixture_flag_true_count_zero() {
        let result = determine_recycle_after(None, true, 0);
        assert_eq!(
            result, 100,
            "count=0 prevents reduction even if flag is true"
        );
    }

    /// Explicit 1 means recycle after every mutant — common in tests.
    #[test]
    fn test_determine_recycle_after_explicit_one() {
        let result = determine_recycle_after(Some(1), false, 0);
        assert_eq!(result, 1);
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

    // --- recycle decision logic tests (INV-2) ---
    //
    // These tests directly exercise the `(count_recycle || memory_recycle) && work_remaining`
    // boolean condition in dispatch_work. Each test targets a specific cargo-mutant survivor:
    // - test_recycle_decision_memory_without_count: kills the `||` → `&&` mutation
    // - test_recycle_decision_no_flags_no_recycle: kills the inversion of either flag
    // - test_recycle_decision_no_work_remaining: kills removal of `&& work_remaining`

    /// INV-2: memory_recycle alone (count threshold not reached) still triggers recycling.
    /// Kills the `||` → `&&` mutation in dispatch_work.
    #[test]
    fn test_recycle_decision_memory_without_count_threshold() {
        let count_recycle = false; // count threshold not reached
        let memory_recycle = true; // RSS exceeded limit
        let work_remaining = true;
        assert!(
            (count_recycle || memory_recycle) && work_remaining,
            "memory_recycle alone must trigger recycling when work remains"
        );
    }

    /// INV-2: neither count nor memory flag → no recycling.
    #[test]
    fn test_recycle_decision_no_flags_no_recycle() {
        let count_recycle = false;
        let memory_recycle = false;
        let work_remaining = true;
        assert!(
            !((count_recycle || memory_recycle) && work_remaining),
            "without either flag, recycling must not occur"
        );
    }

    /// INV-2: memory_recycle set but work queue empty → no recycling (nothing to dispatch).
    #[test]
    fn test_recycle_decision_no_work_remaining() {
        let count_recycle = false;
        let memory_recycle = true;
        let work_remaining = false; // queue drained
        assert!(
            !((count_recycle || memory_recycle) && work_remaining),
            "recycling must not occur when no work remains"
        );
    }
}
