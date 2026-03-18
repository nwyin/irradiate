//! Full mutation testing pipeline: mutate → stats → validate → test → report.

use crate::codegen::mutate_file;
use crate::harness;
use crate::orchestrator::{run_worker_pool, PoolConfig};
use crate::protocol::{MutantResult, MutantStatus, WorkItem};
use crate::stats;
use crate::stats::TestStats;
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Configuration for a mutation testing run.
pub struct RunConfig {
    pub paths_to_mutate: PathBuf,
    pub tests_dir: String,
    pub workers: usize,
    pub timeout_multiplier: f64,
    pub no_stats: bool,
    pub covered_only: bool,
    pub python: PathBuf,
    pub mutant_filter: Option<Vec<String>>,
    /// Respawn workers after this many mutants (0 = disabled).
    pub worker_recycle_after: usize,
    /// Run each mutant in a fresh subprocess instead of the worker pool.
    /// Slower but provides perfect isolation — no pytest state can leak between mutants.
    pub isolate: bool,
}

/// Per-file metadata, mutmut-compatible.
#[derive(Debug, Default, Serialize, Deserialize)]
struct FileMeta {
    exit_code_by_key: HashMap<String, i32>,
    #[serde(default)]
    durations_by_key: HashMap<String, f64>,
}

/// Run the full mutation testing pipeline.
pub async fn run(config: RunConfig) -> Result<()> {
    let project_dir = std::env::current_dir()?;
    let mutants_dir = project_dir.join("mutants");

    // Phase 1: Mutation generation
    eprintln!("Generating mutants...");
    let start = Instant::now();
    let all_mutant_names = generate_mutants(&config.paths_to_mutate, &mutants_dir)?;
    let gen_time = start.elapsed();
    eprintln!(
        "  done in {:.0}ms ({} mutants across {} files)",
        gen_time.as_millis(),
        all_mutant_names.values().map(|v| v.len()).sum::<usize>(),
        all_mutant_names.len(),
    );

    if all_mutant_names.is_empty() {
        eprintln!("No mutations found.");
        return Ok(());
    }

    // Flatten to list of (mutant_name, source_file) for work dispatch
    let mut all_mutants: Vec<(String, String)> = Vec::new();
    for (file, names) in &all_mutant_names {
        for name in names {
            all_mutants.push((name.clone(), file.clone()));
        }
    }

    // Apply filter if specific mutants requested
    if let Some(ref filter) = config.mutant_filter {
        all_mutants.retain(|(name, _)| filter.iter().any(|f| name.contains(f)));
        if all_mutants.is_empty() {
            eprintln!("No mutants match the filter.");
            return Ok(());
        }
    }

    let total_mutants = all_mutants.len();

    // Extract harness
    let harness_dir = harness::extract_harness(&project_dir)?;

    // Build PYTHONPATH once — all subprocess invocations use this same string
    // so that import resolution is identical everywhere (INV-1, INV-2).
    // mutants_dir is handled by the MutantFinder import hook, not PYTHONPATH.
    let pythonpath = build_pythonpath(&harness_dir, &config.paths_to_mutate);

    // Phase 2: Stats collection
    let test_stats = if config.no_stats {
        None
    } else {
        eprintln!("Running stats...");
        let start = Instant::now();
        let s = stats::collect_stats(
            &config.python,
            &project_dir,
            &pythonpath,
            &mutants_dir,
            &config.tests_dir,
        )
        .context("Stats collection failed")?;
        eprintln!("  done in {:.0}ms", start.elapsed().as_millis());
        Some(s)
    };

    // Phase 3: Validation
    eprintln!("Running clean tests...");
    validate_clean_run(
        &config.python,
        &project_dir,
        &pythonpath,
        &mutants_dir,
        &config.tests_dir,
    )?;
    eprintln!("  done");

    eprintln!("Running forced-fail validation...");
    validate_fail_run(
        &config.python,
        &project_dir,
        &pythonpath,
        &mutants_dir,
        &config.tests_dir,
    )?;
    eprintln!("  done");

    // Phase 4: Mutation testing
    if config.isolate {
        eprintln!("Running mutation testing ({total_mutants} mutants, isolated mode)...");
    } else {
        eprintln!(
            "Running mutation testing ({total_mutants} mutants, {} workers)...",
            config.workers
        );
    }
    let start = Instant::now();

    // Build work items
    let work_items: Vec<WorkItem> = all_mutants
        .iter()
        .filter_map(|(mutant_name, _file)| {
            let test_ids = if let Some(ref stats) = test_stats {
                // Extract the function key from mutant name: "module.x_func__irradiate_N" → "module.x_func"
                let func_key = mutant_name
                    .rsplit_once("__irradiate_")
                    .map(|(prefix, _)| prefix)
                    .unwrap_or(mutant_name);
                let tests = stats.tests_for_function(func_key);
                if tests.is_empty() && config.covered_only {
                    return None; // skip uncovered
                }
                if tests.is_empty() {
                    // No coverage info — run all tests
                    stats.duration_by_test.keys().cloned().collect()
                } else {
                    tests
                }
            } else {
                // No stats — will be filled by worker's collected tests
                vec![]
            };
            Some(WorkItem {
                mutant_name: mutant_name.clone(),
                test_ids,
            })
        })
        .collect();

    // For no-stats mode, we need all test IDs — collect them from a dummy pytest run
    let work_items = if config.no_stats {
        // Use all tests discovered by the worker
        let all_tests = discover_tests(
            &config.python,
            &project_dir,
            &pythonpath,
            &mutants_dir,
            &config.tests_dir,
        )?;
        work_items
            .into_iter()
            .map(|mut item| {
                if item.test_ids.is_empty() {
                    item.test_ids = all_tests.clone();
                }
                item
            })
            .collect()
    } else {
        work_items
    };

    // Handle uncovered mutants (exit_code 33)
    let mut results: Vec<MutantResult> = Vec::new();
    let covered_work: Vec<WorkItem> = work_items
        .into_iter()
        .filter(|item| {
            if item.test_ids.is_empty() {
                results.push(MutantResult {
                    mutant_name: item.mutant_name.clone(),
                    exit_code: 33,
                    duration: 0.0,
                    status: MutantStatus::NoTests,
                });
                false
            } else {
                true
            }
        })
        .collect();

    if !covered_work.is_empty() {
        let run_results = if config.isolate {
            run_isolated(
                &config,
                covered_work,
                &harness_dir,
                &mutants_dir,
                test_stats.as_ref(),
            )
            .await?
        } else {
            let pool_config = PoolConfig {
                num_workers: config.workers,
                python: config.python.clone(),
                project_dir: project_dir.clone(),
                mutants_dir: mutants_dir.clone(),
                tests_dir: PathBuf::from(&config.tests_dir),
                timeout_multiplier: config.timeout_multiplier,
                pythonpath: pythonpath.clone(),
                worker_recycle_after: config.worker_recycle_after,
                ..Default::default()
            };
            run_worker_pool(&pool_config, covered_work).await?
        };
        results.extend(run_results);
    }

    let test_time = start.elapsed();

    // Phase 5: Results
    // Write .meta files
    write_meta_files(&mutants_dir, &all_mutant_names, &results)?;

    // Print summary
    print_summary(&results, test_time.as_secs_f64());

    Ok(())
}

/// Display results from previous run.
pub fn results(show_all: bool) -> Result<()> {
    let mutants_dir = std::env::current_dir()?.join("mutants");
    let all_results = load_all_meta(&mutants_dir)?;

    if all_results.is_empty() {
        eprintln!("No results found. Run `irradiate run` first.");
        return Ok(());
    }

    let mut survived = Vec::new();
    let mut killed = 0;
    let mut no_tests = 0;
    let mut timeout = 0;
    let mut errors = 0;

    for (name, exit_code) in &all_results {
        let status = MutantStatus::from_exit_code(*exit_code, false);
        match status {
            MutantStatus::Survived => survived.push(name.as_str()),
            MutantStatus::Killed => killed += 1,
            MutantStatus::NoTests => no_tests += 1,
            MutantStatus::Timeout => timeout += 1,
            _ => errors += 1,
        }
        if show_all {
            let emoji = status_emoji(status);
            println!("{emoji} {name}");
        }
    }

    if !show_all && !survived.is_empty() {
        eprintln!("Survived mutants:");
        for name in &survived {
            println!("  {name}");
        }
    }

    let total = all_results.len();
    eprintln!(
        "\nTotal: {total}  Killed: {killed}  Survived: {}  No tests: {no_tests}  Timeout: {timeout}  Errors: {errors}",
        survived.len()
    );

    Ok(())
}

/// Show diff for a specific mutant.
pub fn show(mutant_name: &str) -> Result<()> {
    let mutants_dir = std::env::current_dir()?.join("mutants");
    let all_meta = load_all_meta(&mutants_dir)?;

    if !all_meta.iter().any(|(name, _)| name == mutant_name) {
        bail!(
            "Mutant '{mutant_name}' not found. Run `irradiate results --all` to see all mutants."
        );
    }

    // mutant_name = "module.x_func__irradiate_N"
    // We need the module (for file lookup) and the local variant name (for function lookup)
    let (module, local_variant) = mutant_name.split_once('.').unwrap_or(("", mutant_name));
    let (local_func_mangled, _) = local_variant
        .rsplit_once("__irradiate_")
        .unwrap_or((local_variant, ""));
    let orig_name = format!("{local_func_mangled}__irradiate_orig");

    // Find the mutated source file
    let candidates = [
        mutants_dir.join(format!("{}/{}.py", module.replace('.', "/"), "__init__")),
        mutants_dir.join(format!("{}.py", module.replace('.', "/"))),
    ];
    let source_file = candidates
        .iter()
        .find(|p| p.exists())
        .ok_or_else(|| anyhow::anyhow!("Cannot find source file for module '{module}'"))?;

    let content = std::fs::read_to_string(source_file)?;

    let orig_marker = format!("def {orig_name}(");
    let mutant_marker = format!("def {local_variant}(");

    match (
        extract_function(&content, &orig_marker),
        extract_function(&content, &mutant_marker),
    ) {
        (Some(orig), Some(mutant)) => {
            println!("# {mutant_name}");
            for line in diff_lines(&orig, &mutant) {
                println!("{line}");
            }
        }
        _ => {
            bail!(
                "Could not extract functions for '{mutant_name}' from {}",
                source_file.display()
            );
        }
    }

    Ok(())
}

// --- Internal helpers ---

/// Default timeout (seconds) when no per-test baseline is available.
const DEFAULT_SUBPROCESS_TIMEOUT_SECS: f64 = 30.0;

/// Minimum absolute timeout for an isolated subprocess (seconds).
///
/// Each isolated run spawns a fresh `python -m pytest` process. Even for trivially
/// fast tests the subprocess startup + pytest collection overhead is ~1-3 seconds.
/// If the estimated test duration is sub-second we must not let the multiplied timeout
/// drop below this floor, or every mutant will time out before pytest even collects.
const MIN_ISOLATED_TIMEOUT_SECS: f64 = 10.0;

/// Run each mutant in a fresh subprocess, sequentially.
///
/// This bypasses the worker pool entirely. Each mutant gets its own `python -m pytest`
/// invocation with `IRRADIATE_ACTIVE_MUTANT` set in the environment. There is no shared
/// pytest session state — each process starts clean.
///
/// Slower than the worker pool but provides perfect isolation and is useful for debugging.
async fn run_isolated(
    config: &RunConfig,
    work_items: Vec<WorkItem>,
    harness_dir: &Path,
    mutants_dir: &Path,
    test_stats: Option<&TestStats>,
) -> Result<Vec<MutantResult>> {
    let mut results = Vec::new();
    let project_dir = std::env::current_dir()?;
    let pythonpath = build_pythonpath(harness_dir, &config.paths_to_mutate);

    for item in work_items {
        let start = Instant::now();

        // Per-item timeout: multiply estimated test duration by the multiplier, then
        // apply MIN_ISOLATED_TIMEOUT_SECS as an absolute floor.
        //
        // The floor is essential: each isolated run spawns a fresh subprocess and pytest
        // must start, import, collect, and then run — even for microsecond-fast tests
        // that overhead is ~1-3 s. Without the floor, `multiplier × tiny_duration`
        // could be < 1ms and every mutant would time out before pytest even starts.
        let estimated_secs = test_stats
            .map(|s| s.estimated_duration(&item.test_ids))
            .unwrap_or(0.0);
        let timeout_secs = (config.timeout_multiplier * estimated_secs)
            .max(config.timeout_multiplier * DEFAULT_SUBPROCESS_TIMEOUT_SECS)
            .max(MIN_ISOLATED_TIMEOUT_SECS);
        let timeout_duration = std::time::Duration::from_secs_f64(timeout_secs);

        let mut child = tokio::process::Command::new(&config.python)
            .arg("-m")
            .arg("pytest")
            .arg("-x")
            .arg("-q")
            .arg("--no-header")
            .arg("-p")
            .arg("irradiate_harness")
            .args(&item.test_ids)
            .env("PYTHONPATH", &pythonpath)
            .env("IRRADIATE_MUTANTS_DIR", mutants_dir)
            .env("IRRADIATE_ACTIVE_MUTANT", &item.mutant_name)
            .current_dir(&project_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("Failed to spawn subprocess for {}", item.mutant_name))?;

        let (exit_code, timed_out) =
            match tokio::time::timeout(timeout_duration, child.wait()).await {
                Ok(Ok(status)) => (status.code().unwrap_or(-1), false),
                Ok(Err(e)) => return Err(e.into()),
                Err(_elapsed) => {
                    // Timeout: kill the child process
                    let _ = child.kill().await;
                    (-1, true)
                }
            };

        let duration = start.elapsed().as_secs_f64();
        results.push(MutantResult {
            mutant_name: item.mutant_name,
            exit_code,
            duration,
            status: MutantStatus::from_exit_code(exit_code, timed_out),
        });
    }

    Ok(results)
}

/// Construct the PYTHONPATH for all Python subprocesses in the pipeline.
///
/// Order: `harness_dir:source_parent`
///
/// `source_parent` is the parent of `paths_to_mutate`, so sibling module
/// imports in the source tree resolve correctly. For example, if
/// `paths_to_mutate` is `src/mylib`, the parent `src` is added, allowing
/// `import mylib` to find the real (unmutated) sibling packages.
///
/// `mutants_dir` is no longer on PYTHONPATH — the MutantFinder import hook
/// (installed via IRRADIATE_MUTANTS_DIR) handles mutant module resolution
/// before sys.path is consulted.
///
/// If `paths_to_mutate` has no parent (is filesystem root), it falls back to
/// itself.
///
/// All six subprocess invocations (validate_clean_run, validate_fail_run,
/// discover_tests, collect_stats, spawn_worker, run_isolated) must use this
/// function so that PYTHONPATH is constructed identically everywhere.
pub fn build_pythonpath(harness_dir: &Path, paths_to_mutate: &Path) -> String {
    let source_parent = paths_to_mutate.parent().unwrap_or(paths_to_mutate);
    format!("{}:{}", harness_dir.display(), source_parent.display())
}

fn generate_mutants(
    paths_to_mutate: &Path,
    mutants_dir: &Path,
) -> Result<HashMap<String, Vec<String>>> {
    // Clean mutants dir
    if mutants_dir.exists() {
        std::fs::remove_dir_all(mutants_dir)?;
    }

    // Walk the source directory for .py files
    let py_files = find_python_files(paths_to_mutate)?;

    // Determine the strip base for computing relative paths.
    //
    // When paths_to_mutate IS a package directory (contains __init__.py), we
    // strip its parent so the package name is preserved in mutants/.
    //   e.g. paths_to_mutate="src/click", file="src/click/types.py"
    //        strip "src/" → rel_path="click/types.py" → mutants/click/types.py  ✓
    //
    // When paths_to_mutate is a source root (no __init__.py), we strip it
    // directly — current behaviour preserved.
    //   e.g. paths_to_mutate="src", file="src/simple_lib/__init__.py"
    //        strip "src/" → rel_path="simple_lib/__init__.py"  ✓
    let strip_base = if paths_to_mutate.join("__init__.py").exists() {
        paths_to_mutate.parent().unwrap_or(paths_to_mutate)
    } else {
        paths_to_mutate
    };

    // Process files in parallel — each writes to a unique path, no conflicts.
    // create_dir_all is safe to call concurrently (handles races internally).
    type MutantEntry = Option<(String, Vec<String>)>;
    let results: Vec<Result<MutantEntry>> = py_files
        .par_iter()
        .map(|py_file| -> Result<MutantEntry> {
            let source = std::fs::read_to_string(py_file)?;

            // Compute module name from file path relative to strip_base.
            // e.g., src/simple_lib/__init__.py with strip_base=src → simple_lib/__init__.py → simple_lib
            // e.g., src/click/types.py with strip_base=src → click/types.py → click.types
            let rel_path = py_file.strip_prefix(strip_base)?;
            let module_name = path_to_module(rel_path);

            if let Some(mutated) = mutate_file(&source, &module_name) {
                // Write mutated file
                let dest = mutants_dir.join(rel_path);
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&dest, &mutated.source)?;

                // Write .meta stub
                let meta_path = PathBuf::from(format!("{}.meta", dest.display()));
                let meta = FileMeta::default();
                std::fs::write(&meta_path, serde_json::to_string_pretty(&meta)?)?;

                Ok(Some((module_name, mutated.mutant_names)))
            } else {
                // No mutations found, but copy the original file verbatim so
                // the full package structure is present in mutants/.  Without
                // this, sibling imports break because Python finds the package
                // in mutants/ (first on PYTHONPATH) but missing modules aren't
                // there.
                let dest = mutants_dir.join(rel_path);
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&dest, &source)?;
                Ok(None)
            }
        })
        .collect();

    // Merge results into HashMap (sequential — avoids mutex on hot path)
    let mut all_names: HashMap<String, Vec<String>> = HashMap::new();
    for result in results {
        if let Some((module, names)) = result? {
            all_names.insert(module, names);
        }
    }

    Ok(all_names)
}

fn find_python_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if !dir.exists() {
        return Ok(files);
    }
    if dir.is_file() && is_mutatable_python_file(dir) {
        files.push(dir.to_path_buf());
        return Ok(files);
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            // Skip __pycache__ and hidden dirs
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.starts_with('.') || name == "__pycache__" {
                continue;
            }
            files.extend(find_python_files(&path)?);
        } else if is_mutatable_python_file(&path) {
            files.push(path);
        }
    }
    Ok(files)
}

fn is_mutatable_python_file(path: &Path) -> bool {
    if path.extension().is_none_or(|e| e != "py") {
        return false;
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    name != "conftest.py"
}

fn path_to_module(rel_path: &Path) -> String {
    let s = rel_path
        .with_extension("")
        .to_string_lossy()
        .replace(['/', '\\'], ".");
    // Strip __init__ suffix
    s.strip_suffix(".__init__").unwrap_or(&s).to_string()
}

fn validate_clean_run(
    python: &Path,
    project_dir: &Path,
    pythonpath: &str,
    mutants_dir: &Path,
    tests_dir: &str,
) -> Result<()> {
    let output = std::process::Command::new(python)
        .arg("-m")
        .arg("pytest")
        .arg("-x")
        .arg("-q")
        .arg("-p")
        .arg("irradiate_harness")
        .arg(tests_dir)
        .env("PYTHONPATH", pythonpath)
        .env("IRRADIATE_MUTANTS_DIR", mutants_dir)
        .current_dir(project_dir)
        .output()
        .context("Failed to run clean test")?;

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Clean test run failed:\n{stdout}\n{stderr}");
    }

    Ok(())
}

fn validate_fail_run(
    python: &Path,
    project_dir: &Path,
    pythonpath: &str,
    mutants_dir: &Path,
    tests_dir: &str,
) -> Result<()> {
    let output = std::process::Command::new(python)
        .arg("-m")
        .arg("pytest")
        .arg("-x")
        .arg("-q")
        .arg("-p")
        .arg("irradiate_harness")
        .arg(tests_dir)
        .env("PYTHONPATH", pythonpath)
        .env("IRRADIATE_MUTANTS_DIR", mutants_dir)
        .env("IRRADIATE_ACTIVE_MUTANT", "fail")
        .current_dir(project_dir)
        .output()
        .context("Failed to run forced-fail validation")?;

    if output.status.success() {
        bail!(
            "Forced-fail validation failed: tests passed when they should have failed. \
             The trampoline may not be wired correctly."
        );
    }

    Ok(())
}

fn discover_tests(
    python: &Path,
    project_dir: &Path,
    pythonpath: &str,
    mutants_dir: &Path,
    tests_dir: &str,
) -> Result<Vec<String>> {
    let output = std::process::Command::new(python)
        .arg("-m")
        .arg("pytest")
        .arg("--collect-only")
        .arg("-q")
        .arg("-p")
        .arg("irradiate_harness")
        .arg(tests_dir)
        .env("PYTHONPATH", pythonpath)
        .env("IRRADIATE_MUTANTS_DIR", mutants_dir)
        .current_dir(project_dir)
        .output()
        .context("Failed to collect tests")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let tests: Vec<String> = stdout
        .lines()
        .filter(|line| line.contains("::"))
        .map(|line| line.trim().to_string())
        .collect();

    Ok(tests)
}

fn write_meta_files(
    mutants_dir: &Path,
    all_names: &HashMap<String, Vec<String>>,
    results: &[MutantResult],
) -> Result<()> {
    // Build result lookup
    let result_map: HashMap<&str, &MutantResult> = results
        .iter()
        .map(|r| (r.mutant_name.as_str(), r))
        .collect();

    for (module, names) in all_names {
        let mut meta = FileMeta::default();
        for name in names {
            if let Some(result) = result_map.get(name.as_str()) {
                meta.exit_code_by_key.insert(name.clone(), result.exit_code);
                meta.durations_by_key.insert(name.clone(), result.duration);
            }
        }

        // Find the .meta file for this module
        let module_path = format!("{}/{}.py.meta", module.replace('.', "/"), "__init__");
        let module_path_alt = format!("{}.py.meta", module.replace('.', "/"));
        let meta_file = if mutants_dir.join(&module_path).exists() {
            mutants_dir.join(module_path)
        } else {
            mutants_dir.join(module_path_alt)
        };

        if let Some(parent) = meta_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&meta_file, serde_json::to_string_pretty(&meta)?)?;
    }

    Ok(())
}

fn load_all_meta(mutants_dir: &Path) -> Result<Vec<(String, i32)>> {
    let mut all = Vec::new();
    if !mutants_dir.exists() {
        return Ok(all);
    }

    for entry in walkdir(mutants_dir)? {
        if entry.extension().is_some_and(|e| e == "meta") {
            let content = std::fs::read_to_string(&entry)?;
            if let Ok(meta) = serde_json::from_str::<FileMeta>(&content) {
                for (name, exit_code) in meta.exit_code_by_key {
                    all.push((name, exit_code));
                }
            }
        }
    }

    all.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(all)
}

fn walkdir(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if !dir.exists() {
        return Ok(files);
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            files.extend(walkdir(&path)?);
        } else {
            files.push(path);
        }
    }
    Ok(files)
}

fn print_summary(results: &[MutantResult], elapsed_secs: f64) {
    let mut killed = 0;
    let mut survived = 0;
    let mut no_tests = 0;
    let mut timeout = 0;
    let mut errors = 0;

    for r in results {
        match r.status {
            MutantStatus::Killed => killed += 1,
            MutantStatus::Survived => survived += 1,
            MutantStatus::NoTests => no_tests += 1,
            MutantStatus::Timeout => timeout += 1,
            _ => errors += 1,
        }
    }

    let total = results.len();
    let rate = if elapsed_secs > 0.0 {
        total as f64 / elapsed_secs
    } else {
        0.0
    };

    eprintln!();
    eprintln!(
        "Mutation testing complete ({total} mutants in {elapsed_secs:.1}s, {rate:.0} mutants/sec)"
    );
    eprintln!("  Killed:    {killed}");
    eprintln!("  Survived:  {survived}");
    if no_tests > 0 {
        eprintln!("  No tests:  {no_tests}");
    }
    if timeout > 0 {
        eprintln!("  Timeout:   {timeout}");
    }
    if errors > 0 {
        eprintln!("  Errors:    {errors}");
    }

    if survived > 0 {
        eprintln!();
        eprintln!("Survived mutants:");
        for r in results {
            if r.status == MutantStatus::Survived {
                eprintln!("  {}", r.mutant_name);
            }
        }
    }
}

fn status_emoji(status: MutantStatus) -> &'static str {
    match status {
        MutantStatus::Killed => "🎉",
        MutantStatus::Survived => "🙁",
        MutantStatus::NoTests => "🫥",
        MutantStatus::Timeout => "⏰",
        MutantStatus::TypeCheck => "🧙",
        MutantStatus::Error => "💥",
    }
}

fn extract_function(source: &str, marker: &str) -> Option<String> {
    let start = source.find(marker)?;
    let rest = &source[start..];
    let mut lines = Vec::new();
    for (i, line) in rest.lines().enumerate() {
        if i > 0 {
            let trimmed = line.trim_start();
            let indent = line.len() - trimmed.len();
            if !trimmed.is_empty() && indent == 0 {
                break;
            }
        }
        lines.push(line);
    }
    Some(lines.join("\n"))
}

fn diff_lines(original: &str, mutant: &str) -> Vec<String> {
    let mut result = Vec::new();
    let orig_lines: Vec<&str> = original.lines().collect();
    let mut_lines: Vec<&str> = mutant.lines().collect();

    let max_len = orig_lines.len().max(mut_lines.len());
    for i in 0..max_len {
        let orig = orig_lines.get(i).unwrap_or(&"");
        let muta = mut_lines.get(i).unwrap_or(&"");
        if orig == muta {
            result.push(format!(" {orig}"));
        } else {
            if !orig.is_empty() {
                result.push(format!("-{orig}"));
            }
            if !muta.is_empty() {
                result.push(format!("+{muta}"));
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_pythonpath_includes_source_parent() {
        // INV-1: PYTHONPATH must be harness_dir:source_parent (2 components).
        // mutants_dir is NOT on PYTHONPATH — the MutantFinder import hook handles it.
        // If paths_to_mutate is "src/mylib", the parent "src" must be on the path
        // so that `import mylib` (and sibling packages) can be resolved.
        let harness = Path::new("/tmp/harness");
        let paths_to_mutate = Path::new("src/mylib");

        let result = build_pythonpath(harness, paths_to_mutate);

        assert!(
            result.contains("/tmp/harness"),
            "harness dir must be in PYTHONPATH"
        );
        assert!(
            result.contains("src"),
            "source parent must be in PYTHONPATH"
        );
        // "src/mylib" itself must NOT appear — only its parent
        assert!(
            !result.contains("src/mylib"),
            "paths_to_mutate itself must not appear — only its parent"
        );
        // mutants_dir must NOT be in PYTHONPATH — hook handles it
        let parts: Vec<&str> = result.split(':').collect();
        assert_eq!(parts.len(), 2, "PYTHONPATH must have exactly 2 components");
    }

    #[test]
    fn test_build_pythonpath_order() {
        // harness_dir must come first (hook install before test collection),
        // then source_parent for untouched siblings.
        // mutants_dir is handled by MutantFinder hook, NOT in PYTHONPATH.
        let harness = Path::new("/h");
        let paths_to_mutate = Path::new("src/lib");

        let result = build_pythonpath(harness, paths_to_mutate);
        let parts: Vec<&str> = result.split(':').collect();

        assert_eq!(parts.len(), 2, "PYTHONPATH must have exactly 2 components");
        assert_eq!(parts[0], "/h", "harness must be first");
        assert_eq!(parts[1], "src", "source parent must be second");
    }

    #[test]
    fn test_build_pythonpath_root_fallback() {
        // If paths_to_mutate has no parent (e.g. a bare filename with no dir component),
        // it falls back to itself rather than panicking.
        let harness = Path::new("/h");
        // A bare path like "mylib" has no meaningful parent — parent() returns ""
        let paths_to_mutate = Path::new("mylib");

        // Should not panic
        let result = build_pythonpath(harness, paths_to_mutate);
        assert!(!result.is_empty());
        let parts: Vec<&str> = result.split(':').collect();
        assert_eq!(parts.len(), 2, "PYTHONPATH must have exactly 2 components");
    }

    #[test]
    fn test_build_pythonpath_all_six_sites_identical() {
        // INV-1: All six subprocess invocations must produce the same string
        // given the same inputs. This test encodes that invariant by calling
        // build_pythonpath multiple times and asserting equality.
        let harness = Path::new("/tmp/h");
        let src = Path::new("project/src");

        let a = build_pythonpath(harness, src);
        let b = build_pythonpath(harness, src);
        assert_eq!(a, b, "build_pythonpath must be deterministic");
    }

    #[test]
    fn test_build_pythonpath_no_mutants_dir() {
        // mutants_dir must NOT appear in PYTHONPATH — it is passed as
        // IRRADIATE_MUTANTS_DIR env var to activate the MutantFinder hook.
        let harness = Path::new("/tmp/harness");
        let paths_to_mutate = Path::new("src/mylib");
        let mutants_dir_str = "/tmp/mutants";

        let result = build_pythonpath(harness, paths_to_mutate);
        assert!(
            !result.contains(mutants_dir_str),
            "mutants_dir must not be in PYTHONPATH — hook handles it"
        );
    }

    // --- path_to_module tests ---

    #[test]
    fn test_path_to_module_simple() {
        assert_eq!(path_to_module(Path::new("foo.py")), "foo");
    }

    #[test]
    fn test_path_to_module_init() {
        // __init__.py should collapse to the package name
        assert_eq!(path_to_module(Path::new("foo/__init__.py")), "foo");
    }

    #[test]
    fn test_path_to_module_nested() {
        assert_eq!(path_to_module(Path::new("a/b/c.py")), "a.b.c");
    }

    #[test]
    fn test_path_to_module_nested_init() {
        assert_eq!(path_to_module(Path::new("a/b/__init__.py")), "a.b");
    }

    #[test]
    fn test_path_to_module_bare_init() {
        // A bare __init__.py with no parent dir: strip_suffix(".__init__") does not match
        // because the string is just "__init__" (no leading dot), so the raw stem is returned.
        assert_eq!(path_to_module(Path::new("__init__.py")), "__init__");
    }

    // --- extract_function tests ---

    #[test]
    fn test_extract_function_single() {
        let src = "def foo():\n    return 1\n";
        let result = extract_function(src, "def foo():");
        assert_eq!(result, Some("def foo():\n    return 1".to_string()));
    }

    #[test]
    fn test_extract_function_stops_at_top_level() {
        let src = "def foo():\n    return 1\n\ndef bar():\n    return 2\n";
        let result = extract_function(src, "def foo():");
        // Should include foo's body but stop before bar
        let s = result.unwrap();
        assert!(s.contains("def foo():"));
        assert!(s.contains("return 1"));
        assert!(!s.contains("def bar():"));
    }

    #[test]
    fn test_extract_function_first_match_only() {
        // When two functions have the same marker prefix, only the first is returned
        let src = "def foo():\n    return 1\n\ndef foo_extra():\n    return 2\n";
        let result = extract_function(src, "def foo():");
        let s = result.unwrap();
        assert!(!s.contains("foo_extra"));
    }

    #[test]
    fn test_extract_function_no_match() {
        let src = "def bar():\n    return 42\n";
        assert_eq!(extract_function(src, "def foo():"), None);
    }

    #[test]
    fn test_extract_function_nested_blocks() {
        let src = "def foo():\n    if True:\n        for i in range(3):\n            pass\n    return 0\n\nx = 1\n";
        let result = extract_function(src, "def foo():");
        let s = result.unwrap();
        assert!(s.contains("if True:"));
        assert!(s.contains("for i in range(3):"));
        assert!(s.contains("return 0"));
        assert!(!s.contains("x = 1"));
    }

    #[test]
    fn test_extract_function_empty_source() {
        assert_eq!(extract_function("", "def foo():"), None);
    }

    // --- diff_lines tests ---

    #[test]
    fn test_diff_lines_identical() {
        let lines = diff_lines("a\nb\nc", "a\nb\nc");
        assert_eq!(lines, vec![" a", " b", " c"]);
    }

    #[test]
    fn test_diff_lines_single_change() {
        let lines = diff_lines("a\nb\nc", "a\nX\nc");
        assert!(lines.contains(&"-b".to_string()));
        assert!(lines.contains(&"+X".to_string()));
        assert!(lines.contains(&" a".to_string()));
        assert!(lines.contains(&" c".to_string()));
    }

    #[test]
    fn test_diff_lines_added_lines() {
        // mutant has more lines than original
        let lines = diff_lines("a", "a\nb");
        assert!(lines.contains(&" a".to_string()));
        assert!(lines.contains(&"+b".to_string()));
    }

    #[test]
    fn test_diff_lines_removed_lines() {
        // original has more lines than mutant
        let lines = diff_lines("a\nb", "a");
        assert!(lines.contains(&" a".to_string()));
        assert!(lines.contains(&"-b".to_string()));
    }

    #[test]
    fn test_diff_lines_both_empty() {
        assert_eq!(diff_lines("", ""), Vec::<String>::new());
    }

    #[test]
    fn test_diff_lines_one_empty() {
        // original empty, mutant has a line
        let lines = diff_lines("", "hello");
        assert!(lines.contains(&"+hello".to_string()));
    }

    // --- find_python_files tests ---

    #[test]
    fn test_find_python_files_flat_only_py() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.py"), "").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "").unwrap();
        let mut files = find_python_files(tmp.path()).unwrap();
        files.sort();
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("a.py"));
    }

    #[test]
    fn test_find_python_files_recursive() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(tmp.path().join("a.py"), "").unwrap();
        std::fs::write(sub.join("b.py"), "").unwrap();
        let mut files = find_python_files(tmp.path()).unwrap();
        files.sort();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_find_python_files_skips_pycache() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("__pycache__");
        std::fs::create_dir(&cache).unwrap();
        std::fs::write(cache.join("cached.py"), "").unwrap();
        std::fs::write(tmp.path().join("real.py"), "").unwrap();
        let files = find_python_files(tmp.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("real.py"));
    }

    #[test]
    fn test_find_python_files_skips_hidden_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let hidden = tmp.path().join(".hidden");
        std::fs::create_dir(&hidden).unwrap();
        std::fs::write(hidden.join("secret.py"), "").unwrap();
        std::fs::write(tmp.path().join("visible.py"), "").unwrap();
        let files = find_python_files(tmp.path()).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("visible.py"));
    }

    #[test]
    fn test_find_python_files_single_file() {
        let tmp = tempfile::tempdir().unwrap();
        let py = tmp.path().join("single.py");
        std::fs::write(&py, "").unwrap();
        let files = find_python_files(&py).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0], py);
    }

    #[test]
    fn test_find_python_files_nonexistent_path() {
        let files = find_python_files(Path::new("/nonexistent/path/xyz")).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn test_find_python_files_no_py_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("readme.txt"), "").unwrap();
        let files = find_python_files(tmp.path()).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn test_find_python_files_skips_conftest() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("subdir");
        std::fs::create_dir(&sub).unwrap();
        let module = tmp.path().join("module.py");
        let root_conftest = tmp.path().join("conftest.py");
        let nested_conftest = sub.join("conftest.py");
        let lib = sub.join("lib.py");
        std::fs::write(&module, "").unwrap();
        std::fs::write(&root_conftest, "").unwrap();
        std::fs::write(&nested_conftest, "").unwrap();
        std::fs::write(&lib, "").unwrap();

        let mut files = find_python_files(tmp.path()).unwrap();
        files.sort();

        assert!(files.contains(&module));
        assert!(files.contains(&lib));
        assert!(!files.contains(&root_conftest));
        assert!(!files.contains(&nested_conftest));
    }

    // --- write_meta_files + load_all_meta round-trip ---

    #[test]
    fn test_meta_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let mutants_dir = tmp.path();

        // Set up: module "mymod" with two mutant names
        let mut all_names: HashMap<String, Vec<String>> = HashMap::new();
        all_names.insert(
            "mymod".to_string(),
            vec![
                "mymod.x_foo__irradiate_1".to_string(),
                "mymod.x_foo__irradiate_2".to_string(),
            ],
        );

        // Create the alt meta path stub so write_meta_files picks the right path
        // write_meta_files checks `mutants_dir/mymod/__init__.py.meta` first;
        // if absent it falls back to `mutants_dir/mymod.py.meta`.
        // We rely on the fallback (alt path) — no stub needed.

        let results = vec![
            MutantResult {
                mutant_name: "mymod.x_foo__irradiate_1".to_string(),
                exit_code: 1,
                duration: 0.5,
                status: MutantStatus::Killed,
            },
            MutantResult {
                mutant_name: "mymod.x_foo__irradiate_2".to_string(),
                exit_code: 0,
                duration: 0.3,
                status: MutantStatus::Survived,
            },
        ];

        write_meta_files(mutants_dir, &all_names, &results).unwrap();

        let loaded = load_all_meta(mutants_dir).unwrap();
        // load_all_meta returns (name, exit_code) sorted by name
        assert_eq!(loaded.len(), 2);
        let map: HashMap<&str, i32> = loaded.iter().map(|(n, c)| (n.as_str(), *c)).collect();
        assert_eq!(map["mymod.x_foo__irradiate_1"], 1);
        assert_eq!(map["mymod.x_foo__irradiate_2"], 0);
    }

    #[test]
    fn test_meta_round_trip_package() {
        // Test the __init__ path: create the stub so write_meta_files uses the init variant
        let tmp = tempfile::tempdir().unwrap();
        let mutants_dir = tmp.path();

        let mut all_names: HashMap<String, Vec<String>> = HashMap::new();
        all_names.insert(
            "mypkg".to_string(),
            vec!["mypkg.x_bar__irradiate_1".to_string()],
        );

        // Create the __init__.py.meta stub so write_meta_files takes the init branch
        let init_meta_dir = mutants_dir.join("mypkg");
        std::fs::create_dir_all(&init_meta_dir).unwrap();
        std::fs::write(init_meta_dir.join("__init__.py.meta"), "{}").unwrap();

        let results = vec![MutantResult {
            mutant_name: "mypkg.x_bar__irradiate_1".to_string(),
            exit_code: 1,
            duration: 0.1,
            status: MutantStatus::Killed,
        }];

        write_meta_files(mutants_dir, &all_names, &results).unwrap();

        let loaded = load_all_meta(mutants_dir).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].0, "mypkg.x_bar__irradiate_1");
        assert_eq!(loaded[0].1, 1);
    }

    // --- generate_mutants: full source tree mirror ---

    #[test]
    fn test_generate_mutants_copies_unmutated_files() {
        // A file with no functions produces no mutations — but it must still be
        // copied into mutants/ so sibling imports don't break.
        let src_tmp = tempfile::tempdir().unwrap();
        let mutants_tmp = tempfile::tempdir().unwrap();

        // File that WILL produce mutations (has a function with arithmetic)
        std::fs::write(
            src_tmp.path().join("math_ops.py"),
            "def add(a, b):\n    return a + b\n",
        )
        .unwrap();
        // File that will NOT produce mutations (just a constant)
        std::fs::write(src_tmp.path().join("constants.py"), "MAX_RETRIES = 3\n").unwrap();

        generate_mutants(src_tmp.path(), mutants_tmp.path()).unwrap();

        // Both files must be present in mutants/
        assert!(
            mutants_tmp.path().join("math_ops.py").exists(),
            "mutated file must be in mutants/"
        );
        assert!(
            mutants_tmp.path().join("constants.py").exists(),
            "unmutated file must be copied to mutants/"
        );

        // The unmutated file content must match the original exactly
        let original = std::fs::read_to_string(src_tmp.path().join("constants.py")).unwrap();
        let copied = std::fs::read_to_string(mutants_tmp.path().join("constants.py")).unwrap();
        assert_eq!(original, copied, "unmutated file must be copied verbatim");
    }

    #[test]
    fn test_generate_mutants_preserves_package_structure() {
        // Multi-file package: generate_mutants must mirror all .py files so that
        // `import pkg.utils` works when pkg is loaded from mutants/.
        let src_tmp = tempfile::tempdir().unwrap();
        let mutants_tmp = tempfile::tempdir().unwrap();

        let pkg = src_tmp.path().join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();

        // __init__.py — no functions, no mutations
        std::fs::write(pkg.join("__init__.py"), "").unwrap();
        // core.py — has a function, will produce mutations
        std::fs::write(pkg.join("core.py"), "def process(x):\n    return x + 1\n").unwrap();
        // utils.py — only constants, no mutations
        std::fs::write(pkg.join("utils.py"), "TIMEOUT = 30\n").unwrap();

        generate_mutants(src_tmp.path(), mutants_tmp.path()).unwrap();

        let mutants_pkg = mutants_tmp.path().join("pkg");
        assert!(
            mutants_pkg.join("__init__.py").exists(),
            "pkg/__init__.py must be in mutants/"
        );
        assert!(
            mutants_pkg.join("core.py").exists(),
            "pkg/core.py must be in mutants/"
        );
        assert!(
            mutants_pkg.join("utils.py").exists(),
            "pkg/utils.py must be in mutants/"
        );

        // utils.py content must be verbatim
        let original = std::fs::read_to_string(pkg.join("utils.py")).unwrap();
        let mirrored = std::fs::read_to_string(mutants_pkg.join("utils.py")).unwrap();
        assert_eq!(
            original, mirrored,
            "unmutated utils.py must be copied verbatim"
        );
    }

    // --- generate_mutants: strip_base / path-flattening regression tests ---

    #[test]
    fn test_generate_mutants_package_dir_preserves_package_name() {
        // INV-1: When paths_to_mutate IS a package directory (has __init__.py),
        // mutant files must be placed under mutants/<package_name>/, not directly
        // under mutants/.  This is the regression case for the original bug where
        // strip_prefix(paths_to_mutate) would yield "types.py" instead of
        // "click/types.py" when paths_to_mutate="src/click".
        let tmp = tempfile::tempdir().unwrap();
        let mutants_tmp = tempfile::tempdir().unwrap();

        // Build src/mypkg/__init__.py  and  src/mypkg/types.py
        let src = tmp.path().join("src");
        let pkg = src.join("mypkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("__init__.py"), "").unwrap();
        std::fs::write(pkg.join("types.py"), "def parse(x):\n    return x + 1\n").unwrap();

        // paths_to_mutate points directly at the package directory "src/mypkg"
        generate_mutants(&pkg, mutants_tmp.path()).unwrap();

        // Files must land under mutants/mypkg/, not directly in mutants/
        let mutants_pkg = mutants_tmp.path().join("mypkg");
        assert!(
            mutants_pkg.join("__init__.py").exists(),
            "mypkg/__init__.py must be in mutants/mypkg/, not mutants/"
        );
        assert!(
            mutants_pkg.join("types.py").exists(),
            "mypkg/types.py must be in mutants/mypkg/, not mutants/types.py"
        );

        // Verify that the incorrectly flattened paths do NOT exist
        assert!(
            !mutants_tmp.path().join("types.py").exists(),
            "mutants/types.py must NOT exist — it would shadow stdlib types module"
        );
        assert!(
            !mutants_tmp.path().join("__init__.py").exists(),
            "mutants/__init__.py must NOT exist — package name must be preserved"
        );
    }

    #[test]
    fn test_generate_mutants_source_root_preserves_package_name() {
        // INV-2: When paths_to_mutate is a source root (no __init__.py), current
        // behavior must be preserved — package name appears in mutants/.
        let tmp = tempfile::tempdir().unwrap();
        let mutants_tmp = tempfile::tempdir().unwrap();

        // Build src/simple_lib/__init__.py
        let src = tmp.path().join("src");
        let pkg = src.join("simple_lib");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("__init__.py"), "def f(x):\n    return x + 1\n").unwrap();

        // paths_to_mutate points at the source root "src" (no __init__.py there)
        generate_mutants(&src, mutants_tmp.path()).unwrap();

        // Files must land under mutants/simple_lib/
        assert!(
            mutants_tmp
                .path()
                .join("simple_lib")
                .join("__init__.py")
                .exists(),
            "simple_lib/__init__.py must be in mutants/simple_lib/"
        );
    }

    #[test]
    fn test_generate_mutants_bare_package_dir_no_parent() {
        // Edge case: paths_to_mutate is a bare package dir with no path prefix (e.g. "mylib").
        // parent() returns "" — strip_prefix("") keeps the full path, which means
        // the package name is preserved.
        let tmp = tempfile::tempdir().unwrap();
        let mutants_tmp = tempfile::tempdir().unwrap();

        // Simulate by using tmp path directly as the package (it IS a package — has __init__.py)
        std::fs::write(tmp.path().join("__init__.py"), "").unwrap();
        std::fs::write(tmp.path().join("core.py"), "def f(x):\n    return x + 1\n").unwrap();

        // Should not panic even when parent() returns an empty component
        let result = generate_mutants(tmp.path(), mutants_tmp.path());
        assert!(
            result.is_ok(),
            "should not fail for package dirs with no parent prefix"
        );
    }
}
