use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tracing::info;

/// Default timeout for the stats collection subprocess (seconds).
const STATS_TIMEOUT_SECS: u64 = 300;

/// Stats collected from running the test suite with `active_mutant = "stats"`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TestStats {
    /// Map from function key (e.g., "module.x_func") to list of test nodeids that exercise it.
    pub tests_by_function: HashMap<String, Vec<String>>,
    /// Map from test nodeid to its execution duration in seconds.
    pub duration_by_test: HashMap<String, f64>,
    /// Pytest exit status from the stats run (0 = pass, 1 = some failures, 2+ = error).
    #[serde(default)]
    pub exit_status: Option<i32>,
    /// Number of test items collected.
    #[serde(default)]
    pub test_count: Option<usize>,
    /// Whether the in-process fail probe succeeded.
    #[serde(default)]
    pub fail_validated: Option<bool>,
}

impl TestStats {
    /// Load stats from a JSON file.
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path).context("Failed to read stats file")?;
        serde_json::from_str(&content).context("Failed to parse stats JSON")
    }

    /// Get the test IDs that cover a given function.
    pub fn tests_for_function(&self, func_key: &str) -> Vec<String> {
        self.tests_by_function
            .get(func_key)
            .cloned()
            .unwrap_or_default()
    }

    /// Get the test IDs that cover a given function, sorted by ascending duration
    /// (shortest first). Tests with unknown duration sort last.
    pub fn tests_for_function_by_duration(&self, func_key: &str) -> Vec<String> {
        let mut tests = self.tests_for_function(func_key);
        tests.sort_by(|a, b| {
            let da = self.duration_by_test.get(a).copied().unwrap_or(f64::MAX);
            let db = self.duration_by_test.get(b).copied().unwrap_or(f64::MAX);
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        });
        tests
    }

    /// Return all known test IDs sorted by ascending duration.
    pub fn all_tests_by_duration(&self) -> Vec<String> {
        let mut tests: Vec<String> = self.duration_by_test.keys().cloned().collect();
        tests.sort_by(|a, b| {
            let da = self.duration_by_test.get(a).copied().unwrap_or(f64::MAX);
            let db = self.duration_by_test.get(b).copied().unwrap_or(f64::MAX);
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        });
        tests
    }

    /// Estimate the total duration for running a set of tests.
    pub fn estimated_duration(&self, test_ids: &[String]) -> f64 {
        test_ids
            .iter()
            .filter_map(|id| self.duration_by_test.get(id))
            .sum()
    }
}

/// Compute a SHA256 fingerprint of all source and test files.
///
/// If the fingerprint matches the cached value, stats collection can be skipped.
/// The fingerprint covers: sorted list of (relative_path, file_content_hash) for
/// all .py files under `paths_to_mutate` and `tests_dir`.
fn compute_stats_fingerprint(
    project_dir: &Path,
    paths_to_mutate: &[PathBuf],
    tests_dir: &str,
) -> String {
    let mut hasher = Sha256::new();

    // Collect all .py files from source paths and test dir
    let mut file_entries: Vec<(String, Vec<u8>)> = Vec::new();

    let mut dirs: Vec<PathBuf> = paths_to_mutate.to_vec();
    let test_path = project_dir.join(tests_dir);
    if test_path.exists() {
        dirs.push(test_path);
    }

    for dir in &dirs {
        let resolved = if dir.is_absolute() {
            dir.clone()
        } else {
            project_dir.join(dir)
        };
        collect_py_files(&resolved, project_dir, &mut file_entries);
    }

    // Sort for deterministic ordering
    file_entries.sort_by(|a, b| a.0.cmp(&b.0));

    for (rel_path, content) in &file_entries {
        // Include path so renames invalidate the cache
        hasher.update((rel_path.len() as u64).to_le_bytes());
        hasher.update(rel_path.as_bytes());
        hasher.update((content.len() as u64).to_le_bytes());
        hasher.update(content);
    }

    format!("{:x}", hasher.finalize())
}

fn collect_py_files(dir: &Path, project_dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip hidden dirs and __pycache__
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !name_str.starts_with('.') && name_str != "__pycache__" {
                collect_py_files(&path, project_dir, out);
            }
        } else if path.extension().is_some_and(|e| e == "py") {
            let rel = path
                .strip_prefix(project_dir)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            if let Ok(content) = std::fs::read(&path) {
                out.push((rel, content));
            }
        }
    }
}

/// Try to load cached stats if the fingerprint matches.
///
/// Returns `Some(stats)` on cache hit, `None` on miss.
pub fn load_cached_stats(
    project_dir: &Path,
    paths_to_mutate: &[PathBuf],
    tests_dir: &str,
) -> Option<TestStats> {
    let irr_dir = project_dir.join(".irradiate");
    let stats_path = irr_dir.join("stats.json");
    let fingerprint_path = irr_dir.join("stats_fingerprint");

    if !stats_path.exists() || !fingerprint_path.exists() {
        return None;
    }

    let saved_fingerprint = std::fs::read_to_string(&fingerprint_path).ok()?;
    let current_fingerprint = compute_stats_fingerprint(project_dir, paths_to_mutate, tests_dir);

    if saved_fingerprint.trim() != current_fingerprint {
        info!("Stats fingerprint changed — will re-collect");
        return None;
    }

    info!("Stats fingerprint matches — using cached stats");
    TestStats::load(&stats_path).ok()
}

/// Save the stats fingerprint after a successful collection.
pub fn save_stats_fingerprint(
    project_dir: &Path,
    paths_to_mutate: &[PathBuf],
    tests_dir: &str,
) {
    let fingerprint = compute_stats_fingerprint(project_dir, paths_to_mutate, tests_dir);
    let path = project_dir.join(".irradiate").join("stats_fingerprint");
    let _ = std::fs::write(path, fingerprint);
}

/// Run the test suite with the stats plugin to collect coverage information.
///
/// This runs pytest once with `--irradiate-stats` and `MUTANT_UNDER_TEST=stats`
/// (or rather, sets up the harness so that `active_mutant = "stats"`).
///
/// `pythonpath` must be pre-built by the caller using `pipeline::build_pythonpath`
/// so that all subprocess invocations use identical PYTHONPATH construction logic.
///
/// `mutants_dir` is passed as `IRRADIATE_MUTANTS_DIR` so the MutantFinder import
/// hook activates and loads trampolined modules from mutants/ instead of PYTHONPATH.
pub fn collect_stats(
    python: &Path,
    project_dir: &Path,
    pythonpath: &str,
    mutants_dir: &Path,
    tests_dir: &str,
    extra_pytest_args: &[String],
) -> Result<TestStats> {
    let stats_output = project_dir.join(".irradiate").join("stats.json");
    let parent = stats_output.parent().ok_or_else(|| anyhow::anyhow!("stats output path has no parent directory"))?;
    std::fs::create_dir_all(parent)?;

    info!("Collecting stats with PYTHONPATH={pythonpath}");

    let mut child = Command::new(python)
        .arg("-m")
        .arg("pytest")
        .arg("--irradiate-stats")
        .arg("-p")
        .arg("irradiate_harness")
        .arg("-p")
        .arg("irradiate_harness.stats_plugin")
        .arg("-q")
        .arg(tests_dir)
        .args(extra_pytest_args)
        .env("PYTHONPATH", pythonpath)
        .env("IRRADIATE_MUTANTS_DIR", mutants_dir)
        .env("IRRADIATE_STATS_OUTPUT", &stats_output)
        .current_dir(project_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to start pytest for stats collection")?;

    // Wait with timeout to avoid hanging on projects with slow trampoline overhead.
    let timeout = Duration::from_secs(STATS_TIMEOUT_SECS);
    let start = Instant::now();
    loop {
        match child.try_wait()? {
            Some(_) => break,
            None if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!(
                    "Stats collection timed out after {}s — the test suite may be too slow under the trampoline",
                    STATS_TIMEOUT_SECS
                );
            }
            None => std::thread::sleep(Duration::from_millis(200)),
        }
    }

    let output = child.wait_with_output()
        .context("Failed to collect pytest output after stats run")?;

    // The stats plugin writes exit_status, test_count, and fail_validated into
    // the stats JSON. Pipeline reads those fields for validation. We only bail
    // here if pytest couldn't even start (no output file at all).
    let exit_code = output.status.code().unwrap_or(-1);
    if exit_code > 1 {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        // pytest_sessionfinish may still have written the file — check before bailing
        if !stats_output.exists() {
            anyhow::bail!(
                "Stats collection failed (exit code {exit_code}):\nstdout: {stdout}\nstderr: {stderr}",
            );
        }
        info!("Stats run exited with code {exit_code} — details in stats.json");
    }
    if exit_code == 1 {
        info!("Stats collection completed with some test failures (exit code 1) — this is OK");
    }

    info!(
        "Stats collection complete, loading from {}",
        stats_output.display()
    );

    if stats_output.exists() {
        TestStats::load(&stats_output)
    } else {
        // Stats plugin may not have written anything if no trampolined functions were hit
        Ok(TestStats::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stats_serialization() {
        let stats = TestStats {
            tests_by_function: HashMap::from([(
                "mod.x_foo".to_string(),
                vec!["test_foo".to_string()],
            )]),
            duration_by_test: HashMap::from([("test_foo".to_string(), 0.042)]),
            ..Default::default()
        };

        let json = serde_json::to_string(&stats).unwrap();
        let parsed: TestStats = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.tests_by_function.len(), 1);
        assert_eq!(parsed.duration_by_test.get("test_foo"), Some(&0.042));
    }

    #[test]
    fn test_estimated_duration() {
        let stats = TestStats {
            tests_by_function: HashMap::new(),
            duration_by_test: HashMap::from([
                ("test_a".to_string(), 0.1),
                ("test_b".to_string(), 0.2),
                ("test_c".to_string(), 0.3),
            ]),
            ..Default::default()
        };

        let duration = stats.estimated_duration(&["test_a".to_string(), "test_b".to_string()]);
        assert!((duration - 0.3).abs() < 0.001);
    }

    #[test]
    fn test_tests_for_function_by_duration() {
        let stats = TestStats {
            tests_by_function: HashMap::from([(
                "mod.x_foo".to_string(),
                vec![
                    "test_slow".to_string(),
                    "test_fast".to_string(),
                    "test_mid".to_string(),
                ],
            )]),
            duration_by_test: HashMap::from([
                ("test_slow".to_string(), 1.0),
                ("test_fast".to_string(), 0.01),
                ("test_mid".to_string(), 0.1),
            ]),
            ..Default::default()
        };

        let sorted = stats.tests_for_function_by_duration("mod.x_foo");
        assert_eq!(sorted, vec!["test_fast", "test_mid", "test_slow"]);
    }

    #[test]
    fn test_tests_for_function_by_duration_unknown_last() {
        let stats = TestStats {
            tests_by_function: HashMap::from([(
                "mod.x_foo".to_string(),
                vec!["test_known".to_string(), "test_unknown".to_string()],
            )]),
            duration_by_test: HashMap::from([("test_known".to_string(), 0.5)]),
            ..Default::default()
        };

        let sorted = stats.tests_for_function_by_duration("mod.x_foo");
        assert_eq!(sorted, vec!["test_known", "test_unknown"]);
    }

    #[test]
    fn test_all_tests_by_duration() {
        let stats = TestStats {
            tests_by_function: HashMap::new(),
            duration_by_test: HashMap::from([
                ("test_slow".to_string(), 1.0),
                ("test_fast".to_string(), 0.01),
                ("test_mid".to_string(), 0.1),
            ]),
            ..Default::default()
        };

        let sorted = stats.all_tests_by_duration();
        assert_eq!(sorted, vec!["test_fast", "test_mid", "test_slow"]);
    }

    #[test]
    fn test_tests_for_function() {
        let stats = TestStats {
            tests_by_function: HashMap::from([(
                "mod.x_foo".to_string(),
                vec!["test_a".to_string(), "test_b".to_string()],
            )]),
            duration_by_test: HashMap::new(),
            ..Default::default()
        };

        assert_eq!(stats.tests_for_function("mod.x_foo").len(), 2);
        assert!(stats.tests_for_function("mod.x_bar").is_empty());
    }
}
