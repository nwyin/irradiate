//! Full mutation testing pipeline: mutate → stats → validate → test → report.

use crate::codegen::mutate_file;
use crate::harness;
use crate::orchestrator::{run_worker_pool, PoolConfig};
use crate::protocol::{MutantResult, MutantStatus, WorkItem};
use crate::stats;
use anyhow::{bail, Context, Result};
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
    let pythonpath = build_pythonpath(&harness_dir, &mutants_dir, &config.paths_to_mutate);

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
            &config.tests_dir,
        )
        .context("Stats collection failed")?;
        eprintln!("  done in {:.0}ms", start.elapsed().as_millis());
        Some(s)
    };

    // Phase 3: Validation
    eprintln!("Running clean tests...");
    validate_clean_run(&config.python, &project_dir, &pythonpath, &config.tests_dir)?;
    eprintln!("  done");

    eprintln!("Running forced-fail validation...");
    validate_fail_run(&config.python, &project_dir, &pythonpath, &config.tests_dir)?;
    eprintln!("  done");

    // Phase 4: Mutation testing
    eprintln!(
        "Running mutation testing ({total_mutants} mutants, {} workers)...",
        config.workers
    );
    let start = Instant::now();

    // Build work items
    let work_items: Vec<WorkItem> = all_mutants
        .iter()
        .filter_map(|(mutant_name, _file)| {
            let test_ids = if let Some(ref stats) = test_stats {
                // Extract the function key from mutant name: "module.x_func__mutmut_N" → "module.x_func"
                let func_key = mutant_name
                    .rsplit_once("__mutmut_")
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
        let all_tests = discover_tests(&config.python, &project_dir, &pythonpath, &config.tests_dir)?;
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

        let pool_results = run_worker_pool(&pool_config, covered_work).await?;
        results.extend(pool_results);
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

    // mutant_name = "module.x_func__mutmut_N"
    // We need the module (for file lookup) and the local variant name (for function lookup)
    let (module, local_variant) = mutant_name.split_once('.').unwrap_or(("", mutant_name));
    let (local_func_mangled, _) = local_variant
        .rsplit_once("__mutmut_")
        .unwrap_or((local_variant, ""));
    let orig_name = format!("{local_func_mangled}__mutmut_orig");

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

/// Construct the PYTHONPATH for all Python subprocesses in the pipeline.
///
/// Order: `harness_dir:mutants_dir:source_parent`
///
/// `source_parent` is the parent of `paths_to_mutate`, so sibling module
/// imports in the source tree resolve correctly. For example, if
/// `paths_to_mutate` is `src/mylib`, the parent `src` is added, allowing
/// `import mylib` to find the real (unmutated) sibling packages.
///
/// If `paths_to_mutate` has no parent (is filesystem root), it falls back to
/// itself.
///
/// All five subprocess invocations (validate_clean_run, validate_fail_run,
/// discover_tests, collect_stats, spawn_worker) must use this function so
/// that PYTHONPATH is constructed identically everywhere.
pub fn build_pythonpath(
    harness_dir: &Path,
    mutants_dir: &Path,
    paths_to_mutate: &Path,
) -> String {
    let source_parent = paths_to_mutate.parent().unwrap_or(paths_to_mutate);
    format!(
        "{}:{}:{}",
        harness_dir.display(),
        mutants_dir.display(),
        source_parent.display()
    )
}

fn generate_mutants(
    paths_to_mutate: &Path,
    mutants_dir: &Path,
) -> Result<HashMap<String, Vec<String>>> {
    let mut all_names: HashMap<String, Vec<String>> = HashMap::new();

    // Clean mutants dir
    if mutants_dir.exists() {
        std::fs::remove_dir_all(mutants_dir)?;
    }

    // Walk the source directory for .py files
    let py_files = find_python_files(paths_to_mutate)?;

    for py_file in &py_files {
        let source = std::fs::read_to_string(py_file)?;

        // Compute module name from file path relative to paths_to_mutate
        // e.g., src/simple_lib/__init__.py with paths_to_mutate=src → simple_lib/__init__.py → simple_lib
        let rel_path = py_file.strip_prefix(paths_to_mutate)?;
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

            all_names.insert(module_name, mutated.mutant_names);
        }
    }

    Ok(all_names)
}

fn find_python_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if !dir.exists() {
        return Ok(files);
    }
    if dir.is_file() && dir.extension().is_some_and(|e| e == "py") {
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
        } else if path.extension().is_some_and(|e| e == "py") {
            files.push(path);
        }
    }
    Ok(files)
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
    tests_dir: &str,
) -> Result<()> {
    let output = std::process::Command::new(python)
        .arg("-m")
        .arg("pytest")
        .arg("-x")
        .arg("-q")
        .arg(tests_dir)
        .env("PYTHONPATH", pythonpath)
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
    tests_dir: &str,
) -> Result<()> {
    let output = std::process::Command::new(python)
        .arg("-m")
        .arg("pytest")
        .arg("-x")
        .arg("-q")
        .arg(tests_dir)
        .env("PYTHONPATH", pythonpath)
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
    tests_dir: &str,
) -> Result<Vec<String>> {
    let output = std::process::Command::new(python)
        .arg("-m")
        .arg("pytest")
        .arg("--collect-only")
        .arg("-q")
        .arg(tests_dir)
        .env("PYTHONPATH", pythonpath)
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
        // If paths_to_mutate is "src/mylib", the parent "src" must be on the path
        // so that `import mylib` (and sibling packages) can be resolved.
        let harness = Path::new("/tmp/harness");
        let mutants = Path::new("/tmp/mutants");
        let paths_to_mutate = Path::new("src/mylib");

        let result = build_pythonpath(harness, mutants, paths_to_mutate);

        assert!(result.contains("/tmp/harness"), "harness dir must be in PYTHONPATH");
        assert!(result.contains("/tmp/mutants"), "mutants dir must be in PYTHONPATH");
        assert!(result.contains("src"), "source parent must be in PYTHONPATH");
        // "src/mylib" itself must NOT appear — only its parent
        assert!(!result.contains("src/mylib"), "paths_to_mutate itself must not appear — only its parent");
    }

    #[test]
    fn test_build_pythonpath_order() {
        // harness_dir must come first so harness overrides everything,
        // then mutants_dir so mutated code shadows the originals,
        // then source_parent for untouched siblings.
        let harness = Path::new("/h");
        let mutants = Path::new("/m");
        let paths_to_mutate = Path::new("src/lib");

        let result = build_pythonpath(harness, mutants, paths_to_mutate);
        let parts: Vec<&str> = result.split(':').collect();

        assert_eq!(parts[0], "/h", "harness must be first");
        assert_eq!(parts[1], "/m", "mutants must be second");
        assert_eq!(parts[2], "src", "source parent must be third");
    }

    #[test]
    fn test_build_pythonpath_root_fallback() {
        // If paths_to_mutate has no parent (e.g. a bare filename with no dir component),
        // it falls back to itself rather than panicking.
        let harness = Path::new("/h");
        let mutants = Path::new("/m");
        // A bare path like "mylib" has no meaningful parent — parent() returns ""
        let paths_to_mutate = Path::new("mylib");

        // Should not panic
        let result = build_pythonpath(harness, mutants, paths_to_mutate);
        assert!(!result.is_empty());
        let parts: Vec<&str> = result.split(':').collect();
        assert_eq!(parts.len(), 3);
    }

    #[test]
    fn test_build_pythonpath_all_five_sites_identical() {
        // INV-1: All five subprocess invocations must produce the same string
        // given the same inputs. This test encodes that invariant by calling
        // build_pythonpath multiple times and asserting equality.
        let harness = Path::new("/tmp/h");
        let mutants = Path::new("/tmp/m");
        let src = Path::new("project/src");

        let a = build_pythonpath(harness, mutants, src);
        let b = build_pythonpath(harness, mutants, src);
        assert_eq!(a, b, "build_pythonpath must be deterministic");
    }
}
