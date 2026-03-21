use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use tracing::info;

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

    /// Estimate the total duration for running a set of tests.
    pub fn estimated_duration(&self, test_ids: &[String]) -> f64 {
        test_ids
            .iter()
            .filter_map(|id| self.duration_by_test.get(id))
            .sum()
    }
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
) -> Result<TestStats> {
    let stats_output = project_dir.join(".irradiate").join("stats.json");
    std::fs::create_dir_all(stats_output.parent().unwrap())?;

    info!("Collecting stats with PYTHONPATH={pythonpath}");

    let output = Command::new(python)
        .arg("-m")
        .arg("pytest")
        .arg("--irradiate-stats")
        .arg("-p")
        .arg("irradiate_harness")
        .arg("-p")
        .arg("irradiate_harness.stats_plugin")
        .arg("-q")
        .arg(tests_dir)
        .env("PYTHONPATH", pythonpath)
        .env("IRRADIATE_MUTANTS_DIR", mutants_dir)
        .env("IRRADIATE_STATS_OUTPUT", &stats_output)
        .current_dir(project_dir)
        .output()
        .context("Failed to run pytest for stats collection")?;

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
