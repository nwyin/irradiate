use anyhow::{Context, Result};
use std::collections::HashMap;
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
pub async fn run_worker_pool(config: &PoolConfig, work_items: Vec<WorkItem>) -> Result<Vec<MutantResult>> {
    if work_items.is_empty() {
        return Ok(vec![]);
    }

    // Extract harness
    let harness_dir = harness::extract_harness(&config.project_dir).context("Failed to extract harness")?;

    // Create unix socket in /tmp to avoid macOS path length limit (104 bytes)
    let socket_name = format!("irradiate-{}-{}.sock", std::process::id(), std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos());
    let socket_path = std::env::temp_dir().join(socket_name);
    let _ = std::fs::remove_file(&socket_path);

    let listener = UnixListener::bind(&socket_path).context("Failed to bind unix socket")?;
    info!("Orchestrator listening on {}", socket_path.display());

    let num_workers = config.num_workers.min(work_items.len());

    // Spawn workers
    let mut processes: Vec<Child> = Vec::new();
    for i in 0..num_workers {
        let child = spawn_worker(i, config, &harness_dir, &socket_path)?;
        processes.push(child);
    }

    // Accept connections and dispatch work
    let results = dispatch_work(listener, processes, work_items, config, &harness_dir, &socket_path).await?;

    // Clean up socket
    let _ = std::fs::remove_file(&socket_path);

    Ok(results)
}

fn spawn_worker(id: usize, config: &PoolConfig, harness_dir: &Path, socket_path: &Path) -> Result<Child> {
    let worker_script = harness::worker_script(harness_dir);

    let child = Command::new(&config.python)
        .arg(&worker_script)
        .env("IRRADIATE_SOCKET", socket_path)
        .env("IRRADIATE_MUTANTS_DIR", &config.mutants_dir)
        .env("IRRADIATE_TESTS_DIR", &config.tests_dir)
        .env(
            "PYTHONPATH",
            format!(
                "{}:{}",
                harness_dir.display(),
                config.mutants_dir.display()
            ),
        )
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

    // Accept connections with a timeout
    let num_workers = processes.len();
    let accept_timeout = Duration::from_secs(30);

    for _ in 0..num_workers {
        let worker_id = next_worker_id;
        next_worker_id += 1;

        let (stream, _) = timeout(accept_timeout, listener.accept())
            .await
            .context("Timeout waiting for worker to connect")?
            .context("Failed to accept worker connection")?;

        let (reader, writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut writer = writer;

        // Read the ready message
        let mut line = String::new();
        reader.read_line(&mut line).await.context("Failed to read ready message")?;
        let ready_msg: WorkerMessage = serde_json::from_str(line.trim()).context("Failed to parse ready message")?;

        match &ready_msg {
            WorkerMessage::Ready { pid, .. } => {
                info!("Worker {worker_id} (pid {pid}) connected and ready");
            }
            _ => {
                warn!("Worker {worker_id} sent unexpected first message: {ready_msg:?}");
            }
        }

        // Create a channel for sending messages to this worker
        let (msg_tx, mut msg_rx) = mpsc::channel::<OrchestratorMessage>(8);
        worker_senders.insert(worker_id, msg_tx);
        idle_workers.push(worker_id);

        // Spawn a task to handle this worker's communication
        let event_tx = event_tx.clone();
        let default_timeout = config.default_timeout;
        let timeout_multiplier = config.timeout_multiplier;
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
    }

    // Drop the original event_tx so the channel closes when all spawned tasks finish
    drop(event_tx);

    // Main dispatch loop
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
        if results.len() + active_mutants.len() == 0 && work_queue.is_empty() {
            break;
        }

        // Wait for events
        match event_rx.recv().await {
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
                idle_workers.push(worker_id);
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
                warn!("Worker {worker_id} disconnected");
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
                    info!("Respawning worker to replace {worker_id}");
                    let new_id = next_worker_id;
                    next_worker_id += 1;
                    match spawn_worker(new_id, config, harness_dir, socket_path) {
                        Ok(child) => {
                            processes.push(child);
                            // The new worker will connect and we'd need to accept it.
                            // For simplicity in this version, we continue without the respawned worker.
                            // The remaining workers will pick up the slack.
                            warn!("Respawned worker {new_id} — but hot-accept not yet implemented, continuing with remaining workers");
                        }
                        Err(e) => {
                            error!("Failed to respawn worker: {e}");
                        }
                    }
                }
            }
            None => {
                // All event senders dropped — workers all disconnected
                warn!("All workers disconnected");
                // Record remaining active mutants as errors
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
