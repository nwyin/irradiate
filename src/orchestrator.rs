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
    /// Must include harness_dir, mutants_dir, and the project source parent so
    /// sibling module imports in mutated code resolve correctly.
    pub pythonpath: String,
    /// Respawn workers after this many mutants to prevent pytest state accumulation.
    /// Set to 0 to disable recycling.
    pub worker_recycle_after: usize,
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
            worker_recycle_after: 100,
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
) -> Result<Vec<MutantResult>> {
    if work_items.is_empty() {
        return Ok(vec![]);
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
    let results = dispatch_work(
        listener,
        processes,
        work_items,
        config,
        &harness_dir,
        &socket_path,
    )
    .await?;

    // Clean up socket
    let _ = std::fs::remove_file(&socket_path);

    Ok(results)
}

fn spawn_worker(
    id: usize,
    config: &PoolConfig,
    harness_dir: &Path,
    socket_path: &Path,
) -> Result<Child> {
    let worker_script = harness::worker_script(harness_dir);

    let child = Command::new(&config.python)
        .arg(&worker_script)
        .env("IRRADIATE_SOCKET", socket_path)
        .env("IRRADIATE_MUTANTS_DIR", &config.mutants_dir)
        .env("IRRADIATE_TESTS_DIR", &config.tests_dir)
        .env("PYTHONPATH", &config.pythonpath)
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

        loop {
            tokio::select! {
                msg = msg_rx.recv() => {
                    match msg {
                        Some(msg) => {
                            let json = serde_json::to_string(&msg).unwrap() + "\n";
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
                    match timeout(default_timeout.mul_f64(timeout_multiplier), reader.read_line(&mut line)).await {
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
    });

    msg_tx
}

async fn dispatch_work(
    listener: UnixListener,
    mut processes: Vec<Child>,
    work_items: Vec<WorkItem>,
    config: &PoolConfig,
    harness_dir: &Path,
    socket_path: &Path,
) -> Result<Vec<MutantResult>> {
    let total_items = work_items.len();
    let mut results: Vec<MutantResult> = Vec::with_capacity(total_items);
    let mut work_queue: Vec<WorkItem> = work_items.into_iter().rev().collect(); // reversed so we pop from the end

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

    let recycle_after = config.worker_recycle_after;
    let accept_timeout = Duration::from_secs(30);
    let default_timeout = config.default_timeout;
    let timeout_multiplier = config.timeout_multiplier;

    // Main dispatch loop — accepts initial worker connections and processes events
    loop {
        // Dispatch work to idle workers
        while let Some(&worker_id) = idle_workers.last() {
            if let Some(item) = work_queue.pop() {
                idle_workers.pop();
                active_mutants.insert(worker_id, item.mutant_name.clone());
                if let Some(sender) = worker_senders.get(&worker_id) {
                    let msg = OrchestratorMessage::Run {
                        mutant: item.mutant_name,
                        tests: item.test_ids,
                    };
                    if sender.send(msg).await.is_err() {
                        warn!("Worker {worker_id} channel closed while dispatching");
                        // Put the work item back
                        if let Some(mutant_name) = active_mutants.remove(&worker_id) {
                            // We lost this work item — record as error
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
                                    Ok(WorkerMessage::Ready { pid, .. }) => {
                                        info!("Worker {worker_id} (pid {pid}) connected and ready");
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
                        active_mutants.remove(&worker_id);
                        results.push(result);

                        let count = worker_recycle_counts.entry(worker_id).or_insert(0);
                        *count += 1;

                        if recycle_after > 0 && *count >= recycle_after && !work_queue.is_empty() {
                            // Recycle: send shutdown, spawn a fresh replacement
                            info!("Worker {worker_id}: recycling after {count} mutants");
                            if let Some(sender) = worker_senders.remove(&worker_id) {
                                let _ = sender.send(OrchestratorMessage::Shutdown).await;
                            }
                            worker_recycle_counts.remove(&worker_id);
                            recycled_worker_ids.insert(worker_id);

                            let spawn_id = next_worker_id; // ID the replacement will be assigned on accept
                            match spawn_worker(spawn_id, config, harness_dir, socket_path) {
                                Ok(child) => {
                                    processes.push(child);
                                    pending_accepts += 1;
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
                        if recycled_worker_ids.remove(&worker_id) {
                            // Expected: we asked this worker to shut down for recycling
                            debug!("Worker {worker_id}: recycled cleanly");
                        } else {
                            warn!("Worker {worker_id} disconnected unexpectedly");
                            // Record any active mutant as error
                            if let Some(mutant_name) = active_mutants.remove(&worker_id) {
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
        }
    }

    // Shutdown remaining workers
    for sender in worker_senders.values() {
        let _ = sender.send(OrchestratorMessage::Shutdown).await;
    }

    // Wait for processes to exit
    for mut proc in processes {
        let _ = tokio::time::timeout(Duration::from_secs(5), proc.wait()).await;
    }

    Ok(results)
}
