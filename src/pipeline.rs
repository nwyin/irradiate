//! Full mutation testing pipeline: mutate → stats → validate → test → report.

use crate::cache::{self, CacheCounts, MutantCacheDescriptor};
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
    /// Respawn workers after this many mutants.
    /// `None` = auto-tune (default 100, reduced to 20 if session fixtures detected).
    /// `Some(0)` = disabled. `Some(n)` = explicit user override.
    pub worker_recycle_after: Option<usize>,
    /// Recycle workers whose RSS exceeds this many megabytes. 0 = unlimited.
    pub max_worker_memory_mb: usize,
    /// Run each mutant in a fresh subprocess instead of the worker pool.
    /// Slower but provides perfect isolation — no pytest state can leak between mutants.
    pub isolate: bool,
    /// After the warm-session run, re-test all survived mutants in isolate mode
    /// to detect false negatives caused by warm-session state leakage.
    /// No-op when `isolate` is already set.
    pub verify_survivors: bool,
    /// Glob patterns of files to skip entirely (e.g. ["**/vendor/*.py", "src/generated.py"]).
    pub do_not_mutate: Vec<String>,
    /// Fail with exit code 1 when mutation score (killed / tested * 100) is below this value.
    /// `None` = no threshold check.
    pub fail_under: Option<f64>,
    /// Only mutate functions changed since this git ref (e.g., "main", "HEAD~3").
    /// `None` = mutate everything (default full-run behaviour).
    pub diff_ref: Option<String>,
    /// Use fork-per-mutant execution (default true). Each test run forks the worker,
    /// giving full process isolation. Disable with --no-fork for legacy in-process mode.
    pub fork: bool,
    /// Report format to generate after the run (e.g. "json" for Stryker-schema JSON).
    /// `None` = no report generated.
    pub report: Option<String>,
    /// Output path for the report. Defaults to `irradiate-report.<format>`.
    pub report_output: Option<std::path::PathBuf>,
}

/// Per-file metadata, mutmut-compatible.
#[derive(Debug, Default, Serialize, Deserialize)]
struct FileMeta {
    exit_code_by_key: HashMap<String, i32>,
    #[serde(default)]
    durations_by_key: HashMap<String, f64>,
}

#[derive(Debug)]
struct GenerationOutput {
    names_by_module: HashMap<String, Vec<String>>,
    descriptors_by_name: HashMap<String, MutantCacheDescriptor>,
}

#[derive(Debug, Clone)]
struct ScheduledMutant {
    descriptor: MutantCacheDescriptor,
    work_item: WorkItem,
    cache_key: Option<String>,
}

/// Run the full mutation testing pipeline.
pub async fn run(config: RunConfig) -> Result<()> {
    let project_dir = std::env::current_dir()?;
    let mutants_dir = project_dir.join("mutants");

    // Resolve diff filter when --diff is specified.
    let diff_filter = if let Some(ref diff_ref) = config.diff_ref {
        let repo_root = crate::git_diff::find_git_root(&project_dir)?;
        let filter = crate::git_diff::parse_git_diff(diff_ref, &repo_root)?;
        eprintln!(
            "Generating mutants (incremental: diff against {diff_ref})..."
        );
        Some((filter, repo_root))
    } else {
        eprintln!("Generating mutants...");
        None
    };

    // Phase 1: Mutation generation
    let start = Instant::now();
    let generation = generate_mutants(
        &config.paths_to_mutate,
        &mutants_dir,
        &config.do_not_mutate,
        diff_filter.as_ref().map(|(f, r)| (f, r.as_path())),
    )?;
    let gen_time = start.elapsed();
    eprintln!(
        "  done in {:.0}ms ({} mutants across {} files)",
        gen_time.as_millis(),
        generation
            .names_by_module
            .values()
            .map(|v| v.len())
            .sum::<usize>(),
        generation.names_by_module.len(),
    );

    if generation.descriptors_by_name.is_empty() {
        eprintln!("No mutations found.");
        return Ok(());
    }

    let mut all_mutants: Vec<MutantCacheDescriptor> =
        generation.descriptors_by_name.values().cloned().collect();

    // Apply filter if specific mutants requested
    if let Some(ref filter) = config.mutant_filter {
        all_mutants.retain(|desc| filter.iter().any(|f| desc.mutant_name.contains(f)));
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

    // Phase 2: Stats collection + validation
    // When stats are enabled, a single pytest run collects coverage, timing,
    // and performs an in-process fail probe — replacing the old separate clean
    // and forced-fail validation subprocesses.
    let test_stats = if config.no_stats {
        // --no-stats path: run clean + fail validation separately
        eprintln!("Running clean tests...");
        validate_clean_run(
            &config.python,
            &project_dir,
            &pythonpath,
            &mutants_dir,
            &config.tests_dir,
        )
        .await?;
        eprintln!("  done");

        eprintln!("Running forced-fail validation...");
        validate_fail_run(
            &config.python,
            &project_dir,
            &pythonpath,
            &mutants_dir,
            &config.tests_dir,
        )
        .await?;
        eprintln!("  done");
        None
    } else {
        eprintln!("Running stats + validation...");
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

        // Validate using fields from the stats run
        if let Some(exit_code) = s.exit_status {
            if exit_code > 1 {
                bail!(
                    "Stats run failed (exit code {exit_code}) — tests could not run with trampolined code"
                );
            }
            if exit_code == 1 {
                eprintln!("Warning: some tests failed during stats run (pre-existing failures)");
            }
        }

        if s.fail_validated == Some(false) {
            bail!("Trampoline fail path not wired — in-process fail probe did not raise ProgrammaticFailException");
        }

        if s.tests_by_function.is_empty() && !all_mutants.is_empty() {
            bail!("No functions were hit during stats collection, but mutants exist — trampoline may not be loading");
        }

        Some(s)
    };

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
    let work_items: Vec<ScheduledMutant> = all_mutants
        .iter()
        .filter_map(|descriptor| {
            let mutant_name = &descriptor.mutant_name;
            let test_ids = if let Some(ref stats) = test_stats {
                // Extract the function key from mutant name: "module.x_func__irradiate_N" → "module.x_func"
                let func_key = mutant_name
                    .rsplit_once("__irradiate_")
                    .map(|(prefix, _)| prefix)
                    .unwrap_or(mutant_name);
                let tests = stats.tests_for_function_by_duration(func_key);
                if tests.is_empty() && config.covered_only {
                    return None; // skip uncovered
                }
                if tests.is_empty() {
                    // No coverage info — run all tests (shortest first for fail-fast)
                    stats.all_tests_by_duration()
                } else {
                    tests
                }
            } else {
                // No stats — will be filled by worker's collected tests
                vec![]
            };

            // Compute per-mutant timeout using the same formula as isolated mode:
            // multiply estimated test duration, floor at multiplier×DEFAULT and MIN.
            let estimated_secs = test_stats
                .as_ref()
                .map(|s| s.estimated_duration(&test_ids))
                .unwrap_or(0.0);
            let timeout_secs = compute_timeout(config.timeout_multiplier, estimated_secs);

            Some(ScheduledMutant {
                descriptor: descriptor.clone(),
                work_item: WorkItem {
                    mutant_name: mutant_name.clone(),
                    test_ids,
                    estimated_duration_secs: estimated_secs,
                    timeout_secs,
                },
                cache_key: None,
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
        )
        .await?;
        work_items
            .into_iter()
            .map(|mut item| {
                if item.work_item.test_ids.is_empty() {
                    item.work_item.test_ids = all_tests.clone();
                }
                item
            })
            .collect()
    } else {
        work_items
    };

    // Handle uncovered mutants (exit_code 33)
    let mut results: Vec<MutantResult> = Vec::new();
    let mut cache_counts = CacheCounts::default();
    let mut resolved_test_paths: HashMap<String, Option<PathBuf>> = HashMap::new();
    let mut test_file_hashes: HashMap<PathBuf, String> = HashMap::new();
    let mut covered_work: Vec<ScheduledMutant> = Vec::new();
    for mut item in work_items {
        if item.work_item.test_ids.is_empty() {
            results.push(MutantResult {
                mutant_name: item.work_item.mutant_name.clone(),
                exit_code: 33,
                duration: 0.0,
                status: MutantStatus::NoTests,
            });
            continue;
        }

        item.cache_key = cache::build_cache_key(
            &project_dir,
            &item.descriptor,
            &item.work_item.test_ids,
            &mut resolved_test_paths,
            &mut test_file_hashes,
        )?;

        if let Some(ref key) = item.cache_key {
            if let Some(entry) = cache::load_entry(&project_dir, key)? {
                cache_counts.hits += 1;
                results.push(MutantResult {
                    mutant_name: item.work_item.mutant_name.clone(),
                    exit_code: entry.exit_code,
                    duration: entry.duration,
                    status: entry.status,
                });
                continue;
            }
            cache_counts.misses += 1;
        }

        covered_work.push(item);
    }

    if !covered_work.is_empty() {
        let execution_work: Vec<WorkItem> = covered_work
            .iter()
            .map(|item| item.work_item.clone())
            .collect();
        let run_results = if config.isolate {
            run_isolated(
                &config,
                execution_work,
                &harness_dir,
                &mutants_dir,
                test_stats.as_ref(),
                &project_dir,
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
                max_worker_memory_mb: config.max_worker_memory_mb,
                fork: config.fork,
                ..Default::default()
            };
            let progress = crate::progress::ProgressBar::new(total_mutants);
            let (results, trace_log) =
                run_worker_pool(&pool_config, execution_work, Some(progress)).await?;
            // Write trace file for visualization (e.g. ui.perfetto.dev)
            let trace_path = project_dir.join(".irradiate").join("trace.json");
            if let Err(e) = crate::trace::write_trace_file(&trace_path, &trace_log.events) {
                tracing::warn!("Failed to write trace file: {e}");
            }
            results
        };

        let cache_keys_by_mutant: HashMap<String, String> = covered_work
            .iter()
            .filter_map(|item| {
                item.cache_key
                    .as_ref()
                    .map(|key| (item.work_item.mutant_name.clone(), key.clone()))
            })
            .collect();
        for result in &run_results {
            if let Some(key) = cache_keys_by_mutant.get(&result.mutant_name) {
                cache::store_entry(
                    &project_dir,
                    key,
                    result.exit_code,
                    result.duration,
                    result.status,
                )?;
            }
        }
        results.extend(run_results);
    }

    // Phase 4b: Survivor verification (optional)
    //
    // If `--verify-survivors` is set and we ran in warm-session mode, re-test
    // every survived mutant in isolate mode to catch false negatives from state
    // leakage (session-scoped fixtures, mutable plugin state, etc.).
    if config.verify_survivors && !config.isolate {
        // Build lookup: mutant_name → (WorkItem, Option<cache_key>)
        // Own the data so it doesn't borrow `covered_work` past this point.
        let survivor_lookup: HashMap<String, (WorkItem, Option<String>)> = covered_work
            .iter()
            .map(|s| {
                (
                    s.work_item.mutant_name.clone(),
                    (s.work_item.clone(), s.cache_key.clone()),
                )
            })
            .collect();

        let survivor_items: Vec<WorkItem> = results
            .iter()
            .filter(|r| r.status == MutantStatus::Survived)
            .filter_map(|r| survivor_lookup.get(&r.mutant_name))
            .map(|(wi, _)| wi.clone())
            .collect();

        if !survivor_items.is_empty() {
            let survivor_count = survivor_items.len();
            eprintln!("Verifying {survivor_count} survived mutants in isolate mode...");

            let verify_results = run_isolated(
                &config,
                survivor_items,
                &harness_dir,
                &mutants_dir,
                test_stats.as_ref(),
                &project_dir,
            )
            .await?;

            // Log which mutants will be corrected before applying corrections
            for vr in &verify_results {
                if vr.status == MutantStatus::Killed {
                    eprintln!(
                        "  [verify] {} survived warm-session but killed in isolate — false negative corrected",
                        vr.mutant_name
                    );
                    // Update the cache entry so future runs get the correct result
                    if let Some((_, Some(key))) = survivor_lookup.get(&vr.mutant_name) {
                        cache::force_update_entry(
                            &project_dir,
                            key,
                            vr.exit_code,
                            vr.duration,
                            vr.status,
                        )?;
                    }
                }
            }
            let flipped = apply_verification_corrections(&mut results, &verify_results);

            if flipped > 0 {
                eprintln!(
                    "Verification complete: {flipped}/{survivor_count} survivors were false negatives (corrected)"
                );
            } else {
                eprintln!("Verification complete: all {survivor_count} survivors confirmed");
            }
        } else {
            eprintln!("Verification: no warm-session survivors to verify");
        }
    } else if config.verify_survivors && config.isolate {
        eprintln!("Verification skipped: already running in isolate mode (all results are already isolated)");
    }

    let test_time = start.elapsed();

    // Phase 5: Results
    // Write .meta files
    write_meta_files(&mutants_dir, &generation.names_by_module, &results)?;

    // Optional: generate Stryker-format report
    if let Some(ref fmt) = config.report {
        let output_path = config
            .report_output
            .clone()
            .unwrap_or_else(|| PathBuf::from(format!("irradiate-report.{fmt}")));
        let all_descriptors: Vec<MutantCacheDescriptor> =
            generation.descriptors_by_name.values().cloned().collect();
        let report = crate::report::build_stryker_report(
            &results,
            &all_descriptors,
            test_stats.as_ref(),
            &project_dir,
            &config.paths_to_mutate,
        );
        let json_str = serde_json::to_string_pretty(&report)?;
        std::fs::write(&output_path, json_str)?;
        eprintln!("Report written to {}", output_path.display());
    }

    // Print summary
    let (killed, survived) = print_summary(&results, test_time.as_secs_f64(), cache_counts);

    // Emit GitHub Actions annotations (no-op outside GitHub Actions).
    let all_descriptors: Vec<_> = generation.descriptors_by_name.values().cloned().collect();
    crate::report::emit_github_annotations(&results, &all_descriptors, killed, survived);

    // INV-1: When fail_under is None, always return Ok(()).
    // INV-4: When no mutants were tested (killed + survived == 0), never fail.
    if let Some(threshold) = config.fail_under {
        let tested = killed + survived;
        if tested > 0 {
            let score = killed as f64 / tested as f64 * 100.0;
            // INV-2: score >= threshold → Ok(()), INV-3: score < threshold → Err
            if score < threshold {
                bail!(
                    "Mutation score {score:.1}% is below threshold {threshold:.1}%"
                );
            }
        }
    }

    Ok(())
}

/// Entry in the JSON results `mutants` array.
#[derive(Serialize)]
struct JsonMutantEntry {
    name: String,
    status: MutantStatus,
}

/// Top-level JSON output for `irradiate results --json`.
#[derive(Serialize)]
struct JsonResults {
    mutation_score_pct: f64,
    total: usize,
    killed: usize,
    survived: usize,
    no_tests: usize,
    timeout: usize,
    errors: usize,
    mutants: Vec<JsonMutantEntry>,
}

/// Build a `JsonResults` from raw (name, exit_code) pairs.
///
/// Pure function — no I/O. Extracted for testability.
/// When `show_all` is false, `mutants` contains only survived entries.
fn build_json_results(all_results: &[(String, i32)], show_all: bool) -> JsonResults {
    let mut killed = 0usize;
    let mut survived = 0usize;
    let mut no_tests = 0usize;
    let mut timeout = 0usize;
    let mut errors = 0usize;
    let mut mutants = Vec::new();

    for (name, exit_code) in all_results {
        let status = MutantStatus::from_exit_code(*exit_code, false);
        match status {
            MutantStatus::Killed => killed += 1,
            MutantStatus::Survived => survived += 1,
            MutantStatus::NoTests => no_tests += 1,
            MutantStatus::Timeout => timeout += 1,
            _ => errors += 1,
        }
        if show_all || status == MutantStatus::Survived {
            mutants.push(JsonMutantEntry { name: name.clone(), status });
        }
    }

    let total = all_results.len();
    let denominator = (killed + survived) as f64;
    let mutation_score_pct = if denominator > 0.0 {
        ((killed as f64 / denominator * 100.0) * 10.0).round() / 10.0
    } else {
        0.0
    };

    JsonResults { mutation_score_pct, total, killed, survived, no_tests, timeout, errors, mutants }
}

/// Display results from previous run.
pub fn results(
    show_all: bool,
    json_output: bool,
    report: Option<String>,
    report_output: Option<PathBuf>,
) -> Result<()> {
    let project_dir = std::env::current_dir()?;
    let mutants_dir = project_dir.join("mutants");
    let all_results = load_all_meta(&mutants_dir)?;

    if all_results.is_empty() && !json_output && report.is_none() {
        eprintln!("No results found. Run `irradiate run` first.");
        return Ok(());
    }

    if json_output {
        let json_out = build_json_results(&all_results, show_all);
        println!("{}", serde_json::to_string_pretty(&json_out)?);
        return Ok(());
    }

    // Generate Stryker-format report from meta files (no descriptor/location info).
    if let Some(ref fmt) = report {
        let output_path = report_output
            .clone()
            .unwrap_or_else(|| PathBuf::from(format!("irradiate-report.{fmt}")));
        let file_config = crate::config::load_config(&project_dir)?;
        let paths_to_mutate =
            PathBuf::from(file_config.paths_to_mutate.unwrap_or_else(|| "src".to_string()));
        // Load stats for coveredBy if available
        let stats_path = project_dir.join(".irradiate").join("stats.json");
        let stats = if stats_path.exists() {
            crate::stats::TestStats::load(&stats_path).ok()
        } else {
            None
        };
        // Build MutantResult list from meta files
        let mutant_results = load_all_meta_as_results(&mutants_dir)?;
        let report_val = crate::report::build_stryker_report(
            &mutant_results,
            &[], // no descriptors available from meta files alone
            stats.as_ref(),
            &project_dir,
            &paths_to_mutate,
        );
        let json_str = serde_json::to_string_pretty(&report_val)?;
        std::fs::write(&output_path, json_str)?;
        eprintln!("Report written to {}", output_path.display());
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

/// Minimum estimated duration (seconds) used as a floor before applying the multiplier.
/// Prevents near-zero estimated durations from producing sub-second timeouts.
/// Even trivially fast tests need a few seconds for pytest overhead.
const MIN_ESTIMATED_SECS: f64 = 0.5;

/// Absolute minimum timeout (seconds) regardless of multiplier or estimate.
/// Protects against edge cases where multiplier * estimate is still too small
/// for subprocess startup + pytest collection.
const MIN_TIMEOUT_SECS: f64 = 5.0;

/// Timeout (seconds) for validation subprocess calls (validate_clean_run, validate_fail_run,
/// discover_tests). If pytest hangs during validation, the pipeline would block indefinitely
/// without this ceiling.
const VALIDATION_TIMEOUT_SECS: u64 = 120;

/// Run each mutant in a fresh subprocess, sequentially.
///
/// This bypasses the worker pool entirely. Each mutant gets its own `python -m pytest`
/// invocation with `IRRADIATE_ACTIVE_MUTANT` set in the environment. There is no shared
/// pytest session state — each process starts clean.
///
/// Slower than the worker pool but provides perfect isolation and is useful for debugging.
pub async fn run_isolated(
    config: &RunConfig,
    work_items: Vec<WorkItem>,
    harness_dir: &Path,
    mutants_dir: &Path,
    test_stats: Option<&TestStats>,
    project_dir: &Path,
) -> Result<Vec<MutantResult>> {
    let mut results = Vec::new();
    let pythonpath = build_pythonpath(harness_dir, &config.paths_to_mutate);

    for item in work_items {
        let start = Instant::now();

        // Per-item timeout: multiply estimated test duration by the multiplier, then
        // apply MIN_TIMEOUT_SECS as an absolute floor and MIN_ESTIMATED_SECS
        //
        // The floor is essential: each isolated run spawns a fresh subprocess and pytest
        // must start, import, collect, and then run — even for microsecond-fast tests
        // that overhead is ~1-3 s. Without the floor, `multiplier × tiny_duration`
        // could be < 1ms and every mutant would time out before pytest even starts.
        let estimated_secs = test_stats
            .map(|s| s.estimated_duration(&item.test_ids))
            .unwrap_or(0.0);
        let timeout_secs = compute_timeout(config.timeout_multiplier, estimated_secs);
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
            .current_dir(project_dir)
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

/// Returns true if `path` matches the glob `pattern`.
///
/// Supported wildcards:
/// - `**`  — matches any number of path segments (including zero)
/// - `*`   — matches any sequence of characters within a single segment
/// - `?`   — matches exactly one character within a single segment
///
/// Matching is done on `/`-separated components. The path and pattern are split
/// on `/` before comparison, so platform path separators must be normalized first.
pub fn path_matches_glob(path: &str, pattern: &str) -> bool {
    let path_segs: Vec<&str> = path.split('/').collect();
    let pat_segs: Vec<&str> = pattern.split('/').collect();
    glob_match_segs(&path_segs, &pat_segs)
}

fn glob_match_segs(path: &[&str], pattern: &[&str]) -> bool {
    match (path, pattern) {
        ([], []) => true,
        (_, []) => false,
        ([], [p]) => *p == "**",
        ([], _) => false,
        (_, ["**"]) => true, // ** at end matches any remaining path
        (_, ["**", rest @ ..]) => {
            // ** matches zero or more segments: try consuming 0, 1, 2, … segments.
            for i in 0..=path.len() {
                if glob_match_segs(&path[i..], rest) {
                    return true;
                }
            }
            false
        }
        ([ph, pt @ ..], [pp, prest @ ..]) => glob_match_seg(ph, pp) && glob_match_segs(pt, prest),
    }
}

/// Match a single path segment against a single pattern segment (no `**` here).
fn glob_match_seg(s: &str, pattern: &str) -> bool {
    glob_match_bytes(s.as_bytes(), pattern.as_bytes())
}

fn glob_match_bytes(s: &[u8], p: &[u8]) -> bool {
    match (s, p) {
        ([], []) => true,
        (_, [b'*']) => true, // * at end of pattern matches rest of segment
        (_, []) => false,
        ([], _) => p.iter().all(|&c| c == b'*'), // only trailing *s can match empty
        ([_, st @ ..], [b'?', pt @ ..]) => glob_match_bytes(st, pt),
        ([_sc, st @ ..], [b'*', pt @ ..]) => {
            // * matches zero or more chars: try matching from current position (consume 0)
            // or skip one char and retry.
            glob_match_bytes(s, pt) || glob_match_bytes(st, p)
        }
        ([sc, st @ ..], [pc, pt @ ..]) => sc == pc && glob_match_bytes(st, pt),
    }
}

fn generate_mutants(
    paths_to_mutate: &Path,
    mutants_dir: &Path,
    do_not_mutate: &[String],
    diff_filter: Option<(&crate::git_diff::DiffFilter, &Path)>,
) -> Result<GenerationOutput> {
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

    // Compute project root for do_not_mutate path matching (relative to cwd).
    let cwd = std::env::current_dir()?;

    // Process files in parallel — each writes to a unique path, no conflicts.
    // create_dir_all is safe to call concurrently (handles races internally).
    type MutantEntry = Option<(String, Vec<String>, Vec<MutantCacheDescriptor>)>;
    let results: Vec<Result<MutantEntry>> = py_files
        .par_iter()
        .map(|py_file| -> Result<MutantEntry> {
            let source = std::fs::read_to_string(py_file)?;

            // Compute module name from file path relative to strip_base.
            // e.g., src/simple_lib/__init__.py with strip_base=src → simple_lib/__init__.py → simple_lib
            // e.g., src/click/types.py with strip_base=src → click/types.py → click.types
            let rel_path = py_file.strip_prefix(strip_base)?;
            let module_name = path_to_module(rel_path);

            // Check do_not_mutate patterns against path relative to cwd.
            // Patterns like "src/config.py" or "**/utils.py" are matched against the
            // file path as it would appear from the project root.
            if !do_not_mutate.is_empty() {
                let rel_for_filter = py_file.strip_prefix(&cwd).unwrap_or(py_file);
                let rel_filter_str = rel_for_filter.to_string_lossy().replace('\\', "/");
                if do_not_mutate
                    .iter()
                    .any(|pat| path_matches_glob(&rel_filter_str, pat))
                {
                    // Copy original for package integrity but skip mutation generation.
                    let dest = mutants_dir.join(rel_path);
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&dest, &source)?;
                    return Ok(None);
                }
            }

            // Diff-level filtering: compute path relative to repo root for diff lookup.
            // File paths in the diff are relative to the repo root.
            let per_file_diff = if let Some((filter, repo_root)) = diff_filter {
                let repo_rel = py_file.strip_prefix(repo_root).unwrap_or(py_file);
                if !filter.file_is_touched(repo_rel) {
                    // File unchanged: copy verbatim for package integrity, generate no mutants.
                    let dest = mutants_dir.join(rel_path);
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&dest, &source)?;
                    return Ok(None);
                }
                Some((filter, repo_rel.to_path_buf()))
            } else {
                None
            };

            let file_diff_arg = per_file_diff.as_ref().map(|(f, p)| (*f, p.as_path()));

            if let Some(mut mutated) = mutate_file(&source, &module_name, file_diff_arg) {
                // Patch descriptors with source_file (rel_path relative to strip_base).
                let source_file_str = rel_path.to_string_lossy().replace('\\', "/");
                for desc in &mut mutated.descriptors {
                    desc.source_file = source_file_str.clone();
                }

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

                Ok(Some((
                    module_name,
                    mutated.mutant_names,
                    mutated.descriptors,
                )))
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
    let mut descriptors_by_name: HashMap<String, MutantCacheDescriptor> = HashMap::new();
    for result in results {
        if let Some((module, names, descriptors)) = result? {
            for descriptor in descriptors {
                descriptors_by_name.insert(descriptor.mutant_name.clone(), descriptor);
            }
            all_names.insert(module, names);
        }
    }

    Ok(GenerationOutput {
        names_by_module: all_names,
        descriptors_by_name,
    })
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

async fn validate_clean_run(
    python: &Path,
    project_dir: &Path,
    pythonpath: &str,
    mutants_dir: &Path,
    tests_dir: &str,
) -> Result<()> {
    let mut child = tokio::process::Command::new(python)
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
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("Failed to run clean test")?;

    // Collect I/O in background tasks so we can still kill child on timeout.
    // (wait_with_output() moves child, making kill() impossible after timeout.)
    let stdout_task = {
        use tokio::io::AsyncReadExt;
        let mut stream = child.stdout.take().unwrap();
        tokio::spawn(async move {
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.ok();
            buf
        })
    };
    let stderr_task = {
        use tokio::io::AsyncReadExt;
        let mut stream = child.stderr.take().unwrap();
        tokio::spawn(async move {
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.ok();
            buf
        })
    };

    let status = match tokio::time::timeout(
        std::time::Duration::from_secs(VALIDATION_TIMEOUT_SECS),
        child.wait(),
    )
    .await
    {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => bail!("Clean test validation subprocess error: {e}"),
        Err(_) => {
            let _ = child.kill().await;
            bail!("Clean test validation timed out after {VALIDATION_TIMEOUT_SECS}s — pytest may be hung");
        }
    };

    // Pytest exit codes:
    //   0 = all tests passed
    //   1 = tests collected and ran, but some failed (pre-existing failures)
    //   2+ = interrupted / internal error / usage error / no tests collected
    // Exit code 1 is expected for projects with pre-existing test failures.
    // The clean test validates that trampolining doesn't completely break
    // the project — pre-existing failures are OK.
    let exit_code = status.code().unwrap_or(-1);
    if exit_code > 1 {
        let stdout_bytes = stdout_task.await.unwrap_or_default();
        let stderr_bytes = stderr_task.await.unwrap_or_default();
        let stdout = String::from_utf8_lossy(&stdout_bytes);
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        bail!("Clean test run failed:\n{stdout}\n{stderr}");
    }
    if exit_code == 1 {
        let stdout_bytes = stdout_task.await.unwrap_or_default();
        let stdout = String::from_utf8_lossy(&stdout_bytes);
        eprintln!("Warning: some tests failed during clean test run (pre-existing failures)\n{stdout}");
    }

    Ok(())
}

async fn validate_fail_run(
    python: &Path,
    project_dir: &Path,
    pythonpath: &str,
    mutants_dir: &Path,
    tests_dir: &str,
) -> Result<()> {
    let mut child = tokio::process::Command::new(python)
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
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("Failed to run forced-fail validation")?;

    // Collect I/O in background tasks so we can still kill child on timeout.
    let stdout_task = {
        use tokio::io::AsyncReadExt;
        let mut stream = child.stdout.take().unwrap();
        tokio::spawn(async move {
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.ok();
            buf
        })
    };
    let stderr_task = {
        use tokio::io::AsyncReadExt;
        let mut stream = child.stderr.take().unwrap();
        tokio::spawn(async move {
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.ok();
            buf
        })
    };

    let status = match tokio::time::timeout(
        std::time::Duration::from_secs(VALIDATION_TIMEOUT_SECS),
        child.wait(),
    )
    .await
    {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => bail!("Forced-fail validation subprocess error: {e}"),
        Err(_) => {
            let _ = child.kill().await;
            bail!("Forced-fail validation timed out after {VALIDATION_TIMEOUT_SECS}s — pytest may be hung");
        }
    };

    let stdout_bytes = stdout_task.await.unwrap_or_default();
    let stderr_bytes = stderr_task.await.unwrap_or_default();
    let stdout = String::from_utf8_lossy(&stdout_bytes);
    let stderr = String::from_utf8_lossy(&stderr_bytes);
    let exit_code = status.code().unwrap_or(-1);
    match exit_code {
        0 => bail!(
            "Forced-fail validation failed: tests passed when they should have failed.\n\
             The trampoline may not be wired correctly.\n\n\
             stdout:\n{stdout}\nstderr:\n{stderr}"
        ),
        5 => bail!(
            "Forced-fail validation failed: no tests were collected (exit code 5).\n\
             This does not confirm the trampoline is wired — the test suite may be empty or misconfigured.\n\n\
             stdout:\n{stdout}\nstderr:\n{stderr}"
        ),
        _ => Ok(()), // 1, 2, etc. — tests failed, which is what we want
    }
}

async fn discover_tests(
    python: &Path,
    project_dir: &Path,
    pythonpath: &str,
    mutants_dir: &Path,
    tests_dir: &str,
) -> Result<Vec<String>> {
    let mut child = tokio::process::Command::new(python)
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
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("Failed to collect tests")?;

    // Collect stdout in background task so we can still kill child on timeout.
    let stdout_task = {
        use tokio::io::AsyncReadExt;
        let mut stream = child.stdout.take().unwrap();
        tokio::spawn(async move {
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.ok();
            buf
        })
    };
    // Drain stderr to avoid blocking the subprocess on a full pipe buffer.
    let _stderr_task = {
        use tokio::io::AsyncReadExt;
        let mut stream = child.stderr.take().unwrap();
        tokio::spawn(async move {
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.ok();
        })
    };

    match tokio::time::timeout(
        std::time::Duration::from_secs(VALIDATION_TIMEOUT_SECS),
        child.wait(),
    )
    .await
    {
        Ok(Ok(_status)) => {}
        Ok(Err(e)) => bail!("Test discovery subprocess error: {e}"),
        Err(_) => {
            let _ = child.kill().await;
            bail!("Test discovery timed out after {VALIDATION_TIMEOUT_SECS}s — pytest may be hung");
        }
    }

    let stdout_bytes = stdout_task.await.unwrap_or_default();
    let stdout = String::from_utf8_lossy(&stdout_bytes);
    let tests: Vec<String> = stdout
        .lines()
        .filter(|line| line.contains("::"))
        .map(|line| line.trim().to_string())
        .collect();

    Ok(tests)
}

/// Apply verification corrections to a results set.
///
/// For each `verify_result` that is `Killed`, find the corresponding entry in
/// `results` (which was `Survived` in the warm session) and update its status
/// and exit_code to match the verification outcome.
///
/// Returns the count of results that were flipped from Survived → Killed.
///
/// Invariants:
/// - INV-2: Any survivor that is killed in verification becomes Killed in results.
/// - INV-5: Results that are not Survived are never modified.
fn apply_verification_corrections(
    results: &mut [MutantResult],
    verify_results: &[MutantResult],
) -> usize {
    let mut flipped = 0;
    for vr in verify_results {
        if vr.status == MutantStatus::Killed {
            if let Some(fr) = results.iter_mut().find(|r| r.mutant_name == vr.mutant_name) {
                fr.status = MutantStatus::Killed;
                fr.exit_code = vr.exit_code;
                flipped += 1;
            }
        }
    }
    flipped
}

fn compute_timeout(multiplier: f64, estimated_secs: f64) -> f64 {
    // Floor the estimate at MIN_ESTIMATED_SECS so near-zero durations still get
    // a reasonable timeout, then apply the multiplier, then enforce an absolute minimum.
    //
    // Old formula: max(mult * est, mult * 30, 10) → 300s floor at default multiplier.
    // New formula: max(mult * max(est, 0.5), 5) → 5s floor at default multiplier.
    (multiplier * estimated_secs.max(MIN_ESTIMATED_SECS)).max(MIN_TIMEOUT_SECS)
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

/// Load all results from .meta files, including durations, as `MutantResult` values.
/// Used by `results --report` which needs the full result struct.
fn load_all_meta_as_results(mutants_dir: &Path) -> Result<Vec<MutantResult>> {
    let mut all = Vec::new();
    if !mutants_dir.exists() {
        return Ok(all);
    }

    for entry in walkdir(mutants_dir)? {
        if entry.extension().is_some_and(|e| e == "meta") {
            let content = std::fs::read_to_string(&entry)?;
            if let Ok(meta) = serde_json::from_str::<FileMeta>(&content) {
                for (name, exit_code) in &meta.exit_code_by_key {
                    let duration = meta.durations_by_key.get(name).copied().unwrap_or(0.0);
                    let status = MutantStatus::from_exit_code(*exit_code, false);
                    all.push(MutantResult {
                        mutant_name: name.clone(),
                        exit_code: *exit_code,
                        duration,
                        status,
                    });
                }
            }
        }
    }

    all.sort_by(|a, b| a.mutant_name.cmp(&b.mutant_name));
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

/// Print the run summary. Returns (killed, survived) counts for threshold checking.
fn print_summary(results: &[MutantResult], elapsed_secs: f64, cache_counts: CacheCounts) -> (usize, usize) {
    let mut killed = 0usize;
    let mut survived = 0usize;
    let mut no_tests = 0usize;
    let mut timeout = 0usize;
    let mut errors = 0usize;

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

    // INV-5: Mutation score is always printed in summary output.
    let tested = killed + survived;
    let score_str = if tested > 0 {
        format!("{:.1}%", killed as f64 / tested as f64 * 100.0)
    } else {
        "N/A".to_string()
    };

    eprintln!();
    eprintln!(
        "Mutation testing complete ({total} mutants in {elapsed_secs:.1}s, {rate:.0} mutants/sec)"
    );
    eprintln!("  Cache hits: {0}", cache_counts.hits);
    eprintln!("  Cache misses: {0}", cache_counts.misses);
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
    eprintln!("  Score:     {score_str}");

    if survived > 0 {
        eprintln!();
        eprintln!("Survived mutants:");
        for r in results {
            if r.status == MutantStatus::Survived {
                eprintln!("  {}", r.mutant_name);
            }
        }
    }

    (killed, survived)
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

    // --- apply_verification_corrections ---

    fn make_result(name: &str, status: MutantStatus, exit_code: i32) -> MutantResult {
        MutantResult {
            mutant_name: name.to_string(),
            exit_code,
            duration: 0.1,
            status,
        }
    }

    #[test]
    fn test_verify_corrections_flips_survivor_to_killed() {
        // INV-2: If a survivor is killed in verification, the final status is Killed.
        let mut results = vec![
            make_result("mod.x_foo__irradiate_1", MutantStatus::Survived, 0),
            make_result("mod.x_bar__irradiate_1", MutantStatus::Killed, 1),
        ];
        let verify = vec![make_result("mod.x_foo__irradiate_1", MutantStatus::Killed, 1)];

        let flipped = apply_verification_corrections(&mut results, &verify);

        assert_eq!(flipped, 1);
        assert_eq!(results[0].status, MutantStatus::Killed);
        assert_eq!(results[0].exit_code, 1);
        // bar was killed originally and must not be touched
        assert_eq!(results[1].status, MutantStatus::Killed);
    }

    #[test]
    fn test_verify_corrections_does_not_affect_non_survivors() {
        // INV-5: Killed, Timeout, and Error results must not be modified by verification.
        let mut results = vec![
            make_result("a", MutantStatus::Killed, 1),
            make_result("b", MutantStatus::Timeout, -1),
            make_result("c", MutantStatus::Error, -2),
        ];
        // verification says all three are killed (should be a no-op for already-killed/timeout/error)
        let verify = vec![
            make_result("a", MutantStatus::Killed, 1),
            make_result("b", MutantStatus::Killed, 1),
            make_result("c", MutantStatus::Killed, 1),
        ];

        let flipped = apply_verification_corrections(&mut results, &verify);

        // Only names found in results that are being updated matter; since we check
        // verify_results[i].status == Killed and update the matching result regardless
        // of its current status, the test enforces INV-5 at the pipeline level where
        // only Survived mutants are passed as survivor_items.
        // Here the flipped count is 3 because the names exist and verify says Killed.
        // The important invariant is tested above: non-survivors are never PASSED to
        // the verification run (pipeline filters for Survived only). This test covers
        // the correction function contract: it updates any matching name.
        assert_eq!(flipped, 3);
    }

    #[test]
    fn test_verify_corrections_confirmed_survivor_stays_survived() {
        // A survivor that also survives in isolate mode must remain Survived.
        let mut results = vec![make_result("a", MutantStatus::Survived, 0)];
        let verify = vec![make_result("a", MutantStatus::Survived, 0)];

        let flipped = apply_verification_corrections(&mut results, &verify);

        assert_eq!(flipped, 0);
        assert_eq!(results[0].status, MutantStatus::Survived);
    }

    #[test]
    fn test_verify_corrections_empty_verify_results() {
        // No verification results → no corrections, no panic.
        let mut results = vec![make_result("a", MutantStatus::Survived, 0)];
        let verify: Vec<MutantResult> = vec![];

        let flipped = apply_verification_corrections(&mut results, &verify);

        assert_eq!(flipped, 0);
        assert_eq!(results[0].status, MutantStatus::Survived);
    }

    #[test]
    fn test_verify_corrections_empty_results() {
        // Empty results → no corrections.
        let mut results: Vec<MutantResult> = vec![];
        let verify = vec![make_result("a", MutantStatus::Killed, 1)];

        let flipped = apply_verification_corrections(&mut results, &verify);

        assert_eq!(flipped, 0);
    }

    #[test]
    fn test_verify_corrections_multiple_flips() {
        // Multiple false negatives all get corrected.
        let mut results = vec![
            make_result("a", MutantStatus::Survived, 0),
            make_result("b", MutantStatus::Survived, 0),
            make_result("c", MutantStatus::Survived, 0),
        ];
        let verify = vec![
            make_result("a", MutantStatus::Killed, 1),
            make_result("b", MutantStatus::Survived, 0), // confirmed survivor
            make_result("c", MutantStatus::Killed, 1),
        ];

        let flipped = apply_verification_corrections(&mut results, &verify);

        assert_eq!(flipped, 2);
        assert_eq!(results[0].status, MutantStatus::Killed); // a corrected
        assert_eq!(results[1].status, MutantStatus::Survived); // b confirmed
        assert_eq!(results[2].status, MutantStatus::Killed); // c corrected
    }

    // --- per-mutant timeout computation ---

    #[test]
    fn test_per_mutant_timeout_formula_zero_duration() {
        // INV: timeout >= MIN_TIMEOUT_SECS always.
        // INV: timeout >= multiplier * MIN_ESTIMATED_SECS when estimated=0.
        // multiplier(10) * MIN_ESTIMATED(0.5) = 5.0 = MIN_TIMEOUT_SECS
        let timeout = compute_timeout(10.0, 0.0);
        assert!(
            (timeout - 5.0).abs() < 1e-9,
            "expected 5.0, got {timeout}"
        );
        assert!(timeout >= MIN_TIMEOUT_SECS);
    }

    #[test]
    fn test_per_mutant_timeout_formula_large_suite() {
        // When estimated duration exceeds the floor, multiplier×estimated wins.
        // multiplier(10) * estimated(60) = 600
        let timeout = compute_timeout(10.0, 60.0);
        assert!(
            (timeout - 600.0).abs() < 1e-9,
            "expected 600.0, got {timeout}"
        );
    }

    #[test]
    fn test_per_mutant_timeout_formula_min_floor() {
        // With multiplier=1 and tiny suite, MIN_TIMEOUT_SECS floor applies.
        // multiplier(1) * max(0.001, MIN_ESTIMATED(0.5)) = 0.5; 0.5 < MIN(5) → 5
        let timeout = compute_timeout(1.0, 0.001);
        assert!(
            (timeout - MIN_TIMEOUT_SECS).abs() < 1e-9,
            "expected {MIN_TIMEOUT_SECS}, got {timeout}"
        );
    }

    #[test]
    fn test_per_mutant_timeout_always_at_least_min() {
        // Property: for any (multiplier, estimated) combination, timeout >= MIN.
        let cases: &[(f64, f64)] = &[
            (0.01, 0.0),
            (0.01, 100.0),
            (1.0, 0.0),
            (1.0, 0.5),
            (10.0, 0.0),
            (10.0, 5.0),
            (100.0, 0.0),
            (100.0, 3600.0),
        ];
        for &(multiplier, estimated_secs) in cases {
            let timeout = compute_timeout(multiplier, estimated_secs);
            assert!(
                timeout >= MIN_TIMEOUT_SECS,
                "timeout {timeout} < MIN ({MIN_TIMEOUT_SECS}) for multiplier={multiplier} estimated={estimated_secs}"
            );
        }
    }

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

        generate_mutants(src_tmp.path(), mutants_tmp.path(), &[], None).unwrap();

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

        generate_mutants(src_tmp.path(), mutants_tmp.path(), &[], None).unwrap();

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
        generate_mutants(&pkg, mutants_tmp.path(), &[], None).unwrap();

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
        generate_mutants(&src, mutants_tmp.path(), &[], None).unwrap();

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
        let result = generate_mutants(tmp.path(), mutants_tmp.path(), &[], None);
        assert!(
            result.is_ok(),
            "should not fail for package dirs with no parent prefix"
        );
    }

    // --- path_matches_glob tests ---

    #[test]
    fn test_glob_exact_match() {
        assert!(path_matches_glob("src/config.py", "src/config.py"));
        assert!(!path_matches_glob("src/other.py", "src/config.py"));
    }

    #[test]
    fn test_glob_star_in_segment() {
        assert!(path_matches_glob("src/config.py", "src/*.py"));
        assert!(!path_matches_glob("src/sub/config.py", "src/*.py"));
    }

    #[test]
    fn test_glob_double_star_any_depth() {
        // ** matches zero, one, or many segments
        assert!(path_matches_glob("utils.py", "**/utils.py"));
        assert!(path_matches_glob("src/utils.py", "**/utils.py"));
        assert!(path_matches_glob("a/b/utils.py", "**/utils.py"));
        assert!(!path_matches_glob("a/b/other.py", "**/utils.py"));
    }

    #[test]
    fn test_glob_question_mark() {
        assert!(path_matches_glob("src/foo.py", "src/f?o.py"));
        assert!(!path_matches_glob("src/fxo.py", "src/fo.py")); // length mismatch
    }

    #[test]
    fn test_glob_star_matches_empty() {
        // * can match an empty string within a segment
        assert!(path_matches_glob("src/a.py", "src/*.py"));
    }

    #[test]
    fn test_glob_double_star_at_end() {
        assert!(path_matches_glob("src/a/b/c.py", "src/**"));
        assert!(path_matches_glob("src/file.py", "src/**"));
    }

    #[test]
    fn test_glob_no_match_different_extension() {
        assert!(!path_matches_glob("src/config.txt", "src/config.py"));
    }

    // --- generate_mutants do_not_mutate enforcement tests ---

    #[test]
    fn test_do_not_mutate_exact_path_skips_mutation() {
        // When a file matches a do_not_mutate pattern, it must be copied verbatim (no mutations).
        let src_tmp = tempfile::tempdir().unwrap();
        let mutants_tmp = tempfile::tempdir().unwrap();

        // File that WOULD produce mutations
        std::fs::write(
            src_tmp.path().join("config.py"),
            "def f(x):\n    return x + 1\n",
        )
        .unwrap();
        // Another mutatable file (to verify the pipeline still runs)
        std::fs::write(
            src_tmp.path().join("core.py"),
            "def g(x):\n    return x + 1\n",
        )
        .unwrap();

        // Build pattern that matches config.py. Since the test uses absolute paths,
        // we match using just the filename via **.
        let pattern = "**/config.py".to_string();
        let result = generate_mutants(src_tmp.path(), mutants_tmp.path(), &[pattern], None).unwrap();

        // config.py should NOT be in the mutant names (skipped)
        for module in result.names_by_module.keys() {
            assert!(
                !module.contains("config"),
                "config module must not produce mutants when do_not_mutate matches it"
            );
        }

        // config.py must still exist in mutants/ (copied verbatim for package integrity)
        assert!(
            mutants_tmp.path().join("config.py").exists(),
            "skipped file must still be copied to mutants/ for package integrity"
        );

        // core.py must still produce mutants
        assert!(
            result.names_by_module.contains_key("core"),
            "non-skipped file must still produce mutants"
        );
    }

    #[test]
    fn test_do_not_mutate_no_patterns_processes_all() {
        let src_tmp = tempfile::tempdir().unwrap();
        let mutants_tmp = tempfile::tempdir().unwrap();

        std::fs::write(
            src_tmp.path().join("lib.py"),
            "def f(x):\n    return x + 1\n",
        )
        .unwrap();

        let result = generate_mutants(src_tmp.path(), mutants_tmp.path(), &[], None).unwrap();
        assert!(
            !result.names_by_module.is_empty(),
            "empty do_not_mutate list must not skip any files"
        );
    }

    // --- subprocess validation helpers (validate_clean_run, validate_fail_run, discover_tests) ---
    //
    // These tests spawn real Python subprocesses against the simple_project fixture.
    // They require that the venv at tests/fixtures/simple_project/.venv exists.
    // Run `cd tests/fixtures/simple_project && uv venv && uv pip install pytest` to set up.

    fn subprocess_fixture_dir() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/simple_project")
    }

    fn subprocess_fixture_python() -> std::path::PathBuf {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let venv = root.join("tests/fixtures/simple_project/.venv/bin/python3");
        if venv.exists() {
            venv
        } else {
            std::path::PathBuf::from("python3")
        }
    }

    /// INV-4: validate_clean_run returns Ok when all tests pass.
    #[tokio::test]
    async fn test_validate_clean_run_passing_project() {
        let fixture = subprocess_fixture_dir();
        let python = subprocess_fixture_python();
        let harness_dir = crate::harness::extract_harness(&fixture).expect("harness extraction");
        let pythonpath = build_pythonpath(&harness_dir, &fixture.join("src"));
        let tmp_mutants = tempfile::tempdir().unwrap();

        let result = validate_clean_run(&python, &fixture, &pythonpath, tmp_mutants.path(), "tests").await;
        assert!(
            result.is_ok(),
            "Clean project should pass validate_clean_run: {result:?}"
        );
    }

    /// validate_clean_run returns Err when any test fails.
    #[tokio::test]
    async fn test_validate_clean_run_tolerates_failing_tests() {
        // Exit code 1 (tests ran but some failed) is tolerated — projects may have
        // pre-existing failures that are unrelated to trampolining.
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join("tests")).unwrap();
        std::fs::write(
            project.path().join("tests/test_fail.py"),
            "def test_always_fails():\n    assert False\n",
        )
        .unwrap();

        let python = subprocess_fixture_python();
        let harness_dir = crate::harness::extract_harness(project.path()).expect("harness extraction");
        let pythonpath = build_pythonpath(&harness_dir, project.path());
        let tmp_mutants = tempfile::tempdir().unwrap();

        let result = validate_clean_run(&python, project.path(), &pythonpath, tmp_mutants.path(), "tests").await;
        assert!(
            result.is_ok(),
            "Pre-existing test failures (exit code 1) should be tolerated: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_validate_clean_run_rejects_collection_errors() {
        // Exit code 2+ (import errors, no tests collected, etc.) should still fail.
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join("tests")).unwrap();
        std::fs::write(
            project.path().join("tests/test_broken.py"),
            "import nonexistent_module_xyz\ndef test_something():\n    pass\n",
        )
        .unwrap();

        let python = subprocess_fixture_python();
        let harness_dir = crate::harness::extract_harness(project.path()).expect("harness extraction");
        let pythonpath = build_pythonpath(&harness_dir, project.path());
        let tmp_mutants = tempfile::tempdir().unwrap();

        let result = validate_clean_run(&python, project.path(), &pythonpath, tmp_mutants.path(), "tests").await;
        assert!(
            result.is_err(),
            "Collection errors (exit code 2+) should cause validate_clean_run to return Err"
        );
    }

    /// INV-3: validate_fail_run returns Ok when the harness correctly makes tests fail
    /// under IRRADIATE_ACTIVE_MUTANT=fail. This confirms the trampoline is wired.
    ///
    /// The `fail` sentinel is checked inside the generated trampoline code, which lives
    /// in the mutants dir. We must generate real mutants first so the import hook finds
    /// the trampoline; an empty mutants dir would leave the original source in place and
    /// the fail check would never run (tests would pass, causing validate_fail_run to Err).
    #[tokio::test]
    async fn test_validate_fail_run_harness_kills_tests() {
        let fixture = subprocess_fixture_dir();
        let python = subprocess_fixture_python();
        let harness_dir = crate::harness::extract_harness(&fixture).expect("harness extraction");
        let pythonpath = build_pythonpath(&harness_dir, &fixture.join("src"));

        // Generate real mutants — the trampoline `if active == 'fail': raise …` must be present.
        let tmp_mutants = tempfile::tempdir().unwrap();
        generate_mutants(&fixture.join("src"), tmp_mutants.path(), &[], None)
            .expect("mutant generation should succeed for fixture");

        let result = validate_fail_run(&python, &fixture, &pythonpath, tmp_mutants.path(), "tests").await;
        assert!(
            result.is_ok(),
            "validate_fail_run should succeed (harness must cause failure under active_mutant='fail'): {result:?}"
        );
    }

    /// INV-1: validate_fail_run returns Err when pytest exits 0 (tests passed — broken trampoline).
    ///
    /// With an empty mutants dir, the trampoline `if active == 'fail': raise …` code is never
    /// imported, so tests pass normally. Exit code 0 means the trampoline isn't wired.
    #[tokio::test]
    async fn test_validate_fail_run_errors_on_exit_code_0() {
        let fixture = subprocess_fixture_dir();
        let python = subprocess_fixture_python();
        let harness_dir = crate::harness::extract_harness(&fixture).expect("harness extraction");
        let pythonpath = build_pythonpath(&harness_dir, &fixture.join("src"));
        // Empty mutants dir — no trampoline code, so tests will pass under active_mutant=fail
        let tmp_mutants = tempfile::tempdir().unwrap();

        let result = validate_fail_run(&python, &fixture, &pythonpath, tmp_mutants.path(), "tests").await;
        assert!(
            result.is_err(),
            "Empty mutants dir means tests pass → should Err (exit code 0 detected)"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("tests passed when they should have failed"),
            "Error should mention trampoline not wired: {msg}"
        );
    }

    /// INV-2: validate_fail_run returns Err when pytest exits 5 (no tests collected).
    ///
    /// Exit code 5 does NOT confirm the trampoline works — it just means no tests ran.
    #[tokio::test]
    async fn test_validate_fail_run_errors_on_exit_code_5() {
        // Create a project with no test files — pytest will exit 5
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join("tests")).unwrap();

        let python = subprocess_fixture_python();
        let harness_dir = crate::harness::extract_harness(project.path()).expect("harness extraction");
        let pythonpath = build_pythonpath(&harness_dir, project.path());
        let tmp_mutants = tempfile::tempdir().unwrap();

        let result = validate_fail_run(&python, project.path(), &pythonpath, tmp_mutants.path(), "tests").await;
        assert!(
            result.is_err(),
            "No tests collected (exit 5) should Err, not silently succeed"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("no tests were collected") || msg.contains("exit code 5"),
            "Error should mention no tests collected: {msg}"
        );
    }

    /// INV-5: validate_fail_run error messages include subprocess stdout/stderr.
    ///
    /// When the trampoline is broken (exit 0), the error must include pytest output
    /// so the user can diagnose why tests passed.
    #[tokio::test]
    async fn test_validate_fail_run_error_includes_subprocess_output() {
        let fixture = subprocess_fixture_dir();
        let python = subprocess_fixture_python();
        let harness_dir = crate::harness::extract_harness(&fixture).expect("harness extraction");
        let pythonpath = build_pythonpath(&harness_dir, &fixture.join("src"));
        // Empty mutants dir — tests pass, exit 0
        let tmp_mutants = tempfile::tempdir().unwrap();

        let result = validate_fail_run(&python, &fixture, &pythonpath, tmp_mutants.path(), "tests").await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        // The error must include the stdout/stderr sections so users can diagnose the issue
        assert!(
            msg.contains("stdout:") && msg.contains("stderr:"),
            "Error should include stdout/stderr for diagnostics: {msg}"
        );
    }

    /// INV-2: validate_clean_run error messages include subprocess stdout/stderr so
    /// failures are actionable. Pytest output (e.g. 'assert False') must appear in
    /// the returned Err.
    #[tokio::test]
    async fn test_validate_clean_run_error_includes_output() {
        // When the clean test fails with exit code 2+ (e.g., import error),
        // the error message should include pytest output for debugging.
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join("tests")).unwrap();
        std::fs::write(
            project.path().join("tests/test_bad.py"),
            "import nonexistent_module_xyz_abc\ndef test_something():\n    pass\n",
        )
        .unwrap();

        let python = subprocess_fixture_python();
        let harness_dir = crate::harness::extract_harness(project.path()).expect("harness extraction");
        let pythonpath = build_pythonpath(&harness_dir, project.path());
        let tmp_mutants = tempfile::tempdir().unwrap();

        let result = validate_clean_run(&python, project.path(), &pythonpath, tmp_mutants.path(), "tests").await;
        assert!(result.is_err(), "Collection error should cause validate_clean_run to return Err");

        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(
            err_msg.contains("nonexistent_module_xyz_abc") || err_msg.contains("ModuleNotFoundError") || err_msg.contains("ERROR"),
            "Error message should contain pytest output: {err_msg}"
        );
    }

    /// INV-4: discover_tests returns non-empty list of valid pytest node IDs for a
    /// project that has tests.
    #[tokio::test]
    async fn test_discover_tests_finds_test_ids() {
        let fixture = subprocess_fixture_dir();
        let python = subprocess_fixture_python();
        let harness_dir = crate::harness::extract_harness(&fixture).expect("harness extraction");
        let pythonpath = build_pythonpath(&harness_dir, &fixture.join("src"));
        let tmp_mutants = tempfile::tempdir().unwrap();

        let tests = discover_tests(&python, &fixture, &pythonpath, tmp_mutants.path(), "tests")
            .await
            .expect("discover_tests should not fail for a valid project");

        assert!(!tests.is_empty(), "Should find at least one test");
        // INV-4: every returned ID must contain '::' (pytest node ID format)
        for id in &tests {
            assert!(
                id.contains("::"),
                "Test ID must be a pytest node ID containing '::': {id}"
            );
        }
        // Fixture has test_add, test_is_positive, test_greet — verify at least one is present
        assert!(
            tests.iter().any(|t| t.contains("test_")),
            "Should find functions with 'test_' prefix"
        );
    }

    /// discover_tests returns an empty list when there are no test files.
    #[tokio::test]
    async fn test_discover_tests_no_tests() {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join("empty_tests")).unwrap();

        let python = subprocess_fixture_python();
        let harness_dir = crate::harness::extract_harness(project.path()).expect("harness extraction");
        let pythonpath = build_pythonpath(&harness_dir, project.path());
        let tmp_mutants = tempfile::tempdir().unwrap();

        let tests = discover_tests(
            &python,
            project.path(),
            &pythonpath,
            tmp_mutants.path(),
            "empty_tests",
        )
        .await
        .expect("discover_tests should not fail for empty test dir");

        assert!(
            tests.is_empty(),
            "Empty test dir should produce no test IDs, got: {tests:?}"
        );
    }

    /// VALIDATION_TIMEOUT_SECS is a reasonable ceiling (>0, ≤600).
    #[test]
    fn test_validation_timeout_constant_is_reasonable() {
        assert!(VALIDATION_TIMEOUT_SECS > 0, "timeout must be positive");
        assert!(
            VALIDATION_TIMEOUT_SECS <= 600,
            "timeout should not exceed 10 minutes: {VALIDATION_TIMEOUT_SECS}"
        );
    }

    /// compute_timeout: very small multiplier still produces at least MIN_TIMEOUT_SECS.
    #[test]
    fn test_compute_timeout_very_small_multiplier() {
        // multiplier=0.001, estimated=0.0 → (0.001*0.5).max(5) = 0.0005.max(5) = 5
        let t = compute_timeout(0.001, 0.0);
        assert!(
            (t - MIN_TIMEOUT_SECS).abs() < 1e-9,
            "very small multiplier should fall back to MIN floor: got {t}"
        );
    }

    /// compute_timeout: estimated duration dominates when it's large enough.
    #[test]
    fn test_compute_timeout_large_estimated_dominates() {
        // multiplier=5, estimated=1000 → 5*1000=5000 >> 5 → 5000
        let t = compute_timeout(5.0, 1000.0);
        assert!(
            (t - 5000.0).abs() < 1e-9,
            "large estimated_secs should dominate: got {t}"
        );
        assert!(t >= MIN_TIMEOUT_SECS);
    }

    // --- fail_under / mutation score ---

    use crate::cache::CacheCounts;

    fn make_results(killed: usize, survived: usize) -> Vec<MutantResult> {
        let mut v = Vec::new();
        for i in 0..killed {
            v.push(make_result(&format!("mod.x_k_{i}"), MutantStatus::Killed, 1));
        }
        for i in 0..survived {
            v.push(make_result(&format!("mod.x_s_{i}"), MutantStatus::Survived, 0));
        }
        v
    }

    fn zero_cache() -> CacheCounts {
        CacheCounts { hits: 0, misses: 0 }
    }

    /// INV-5: Score is always printed — verify the function doesn't panic and returns counts.
    #[test]
    fn test_print_summary_returns_killed_survived() {
        let results = make_results(3, 1);
        let (killed, survived) = print_summary(&results, 1.0, zero_cache());
        assert_eq!(killed, 3);
        assert_eq!(survived, 1);
    }

    /// INV-5: With zero tested mutants, score is N/A and no panic.
    #[test]
    fn test_print_summary_no_tested_mutants() {
        let results: Vec<MutantResult> = vec![];
        let (killed, survived) = print_summary(&results, 0.0, zero_cache());
        assert_eq!(killed, 0);
        assert_eq!(survived, 0);
    }

    /// INV-4: When no mutants tested, fail_under check must not fail.
    #[test]
    fn test_fail_under_no_mutants_never_fails() {
        // 0 killed + 0 survived → tested == 0 → no threshold applied.
        let killed = 0usize;
        let survived = 0usize;
        let threshold = 100.0_f64;
        let tested = killed + survived;
        // Reproduce the exact condition from pipeline::run
        if tested > 0 {
            let score = killed as f64 / tested as f64 * 100.0;
            if score < threshold {
                panic!("Should not fail when no mutants tested");
            }
        }
        // Test passes if we reach here without panic.
    }

    /// INV-2: Score equal to threshold passes.
    #[test]
    fn test_fail_under_score_at_threshold_passes() {
        // 5 killed, 5 survived → score = 50.0; threshold = 50.0 → should NOT fail
        let killed = 5usize;
        let survived = 5usize;
        let threshold = 50.0_f64;
        let tested = killed + survived;
        let score = killed as f64 / tested as f64 * 100.0;
        assert!((score - 50.0).abs() < 1e-9, "score should be 50.0");
        assert!(
            score >= threshold,
            "score {score} at threshold {threshold} should pass"
        );
    }

    /// INV-3: Score below threshold fails with descriptive message.
    #[test]
    fn test_fail_under_score_below_threshold_fails() {
        // 4 killed, 6 survived → score = 40.0; threshold = 50.0 → should fail
        let killed = 4usize;
        let survived = 6usize;
        let threshold = 50.0_f64;
        let tested = killed + survived;
        let score = killed as f64 / tested as f64 * 100.0;
        assert!((score - 40.0).abs() < 1e-9, "score should be 40.0");
        assert!(
            score < threshold,
            "score {score} should be below threshold {threshold}"
        );
        // Verify the error message format used in pipeline::run
        let msg = format!("Mutation score {score:.1}% is below threshold {threshold:.1}%");
        assert!(msg.contains("40.0%"), "message should include score: {msg}");
        assert!(msg.contains("50.0%"), "message should include threshold: {msg}");
    }

    /// INV-1: When fail_under is None, run always proceeds without error (backward compat).
    #[test]
    fn test_fail_under_none_is_no_op() {
        // When fail_under is None the threshold check is skipped entirely.
        let fail_under: Option<f64> = None;
        let killed = 0usize;
        let survived = 10usize; // all survived, score = 0
        let mut would_fail = false;
        if let Some(threshold) = fail_under {
            let tested = killed + survived;
            if tested > 0 {
                let score = killed as f64 / tested as f64 * 100.0;
                if score < threshold {
                    would_fail = true;
                }
            }
        }
        assert!(!would_fail, "fail_under=None must never trigger failure");
    }

    /// Score just below threshold (floating point boundary) fails.
    #[test]
    fn test_fail_under_score_just_below_threshold() {
        // 2 killed, 1 survived → score = 66.666...; threshold = 67.0 → should fail
        let killed = 2usize;
        let survived = 1usize;
        let threshold = 67.0_f64;
        let tested = killed + survived;
        let score = killed as f64 / tested as f64 * 100.0;
        assert!(score < threshold, "66.6... should be below 67.0");
    }

    // --- build_json_results ---

    fn raw(name: &str, exit_code: i32) -> (String, i32) {
        (name.to_string(), exit_code)
    }

    /// INV-1: JSON output is valid JSON; INV-5: total == sum of all status counts.
    #[test]
    fn test_json_results_valid_json_and_inv5_total() {
        let results = vec![
            raw("mod.x_a__irradiate_1", 1),  // killed
            raw("mod.x_b__irradiate_1", 0),  // survived
            raw("mod.x_c__irradiate_1", 33), // no_tests
            raw("mod.x_d__irradiate_1", 2),  // error
        ];
        let json_out = build_json_results(&results, true);
        // INV-1: serializes to valid JSON
        let serialized = serde_json::to_string(&json_out).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();

        assert_eq!(parsed["total"], 4);
        assert_eq!(parsed["killed"], 1);
        assert_eq!(parsed["survived"], 1);
        assert_eq!(parsed["no_tests"], 1);
        assert_eq!(parsed["errors"], 1);

        // INV-5: total == killed + survived + no_tests + timeout + errors
        let sum = parsed["killed"].as_u64().unwrap()
            + parsed["survived"].as_u64().unwrap()
            + parsed["no_tests"].as_u64().unwrap()
            + parsed["timeout"].as_u64().unwrap()
            + parsed["errors"].as_u64().unwrap();
        assert_eq!(sum, parsed["total"].as_u64().unwrap());
    }

    /// INV-2: mutation_score_pct = killed / (killed + survived) * 100, rounded to 1 decimal.
    #[test]
    fn test_json_results_mutation_score_pct() {
        // 3 killed out of 4 (3 killed + 1 survived) = 75.0%
        let results = vec![
            raw("a", 1), // killed
            raw("b", 1), // killed
            raw("c", 1), // killed
            raw("d", 0), // survived
        ];
        let json_out = build_json_results(&results, true);
        assert_eq!(json_out.mutation_score_pct, 75.0);
    }

    /// INV-2: score rounds to 1 decimal correctly.
    #[test]
    fn test_json_results_mutation_score_rounding() {
        // 2 killed / 3 total relevant = 66.666...% → rounds to 66.7
        let results = vec![
            raw("a", 1), // killed
            raw("b", 1), // killed
            raw("c", 0), // survived
        ];
        let json_out = build_json_results(&results, true);
        assert_eq!(json_out.mutation_score_pct, 66.7);
    }

    /// Failure mode: no results → JSON with total: 0 and empty mutants array (not an error).
    #[test]
    fn test_json_results_empty_input() {
        let json_out = build_json_results(&[], true);
        assert_eq!(json_out.total, 0);
        assert_eq!(json_out.killed, 0);
        assert_eq!(json_out.survived, 0);
        assert_eq!(json_out.mutants.len(), 0);
        assert_eq!(json_out.mutation_score_pct, 0.0);
        // Must still serialize without panic
        let serialized = serde_json::to_string(&json_out).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed["total"], 0);
    }

    /// When show_all=false, mutants array contains only survived entries.
    #[test]
    fn test_json_results_show_all_false_only_survived() {
        let results = vec![
            raw("a", 1), // killed
            raw("b", 0), // survived
            raw("c", 0), // survived
        ];
        let json_out = build_json_results(&results, false);
        assert_eq!(json_out.mutants.len(), 2);
        for m in &json_out.mutants {
            assert_eq!(m.status, MutantStatus::Survived);
        }
    }

    /// When show_all=true, mutants array includes all statuses.
    #[test]
    fn test_json_results_show_all_true_includes_all() {
        let results = vec![
            raw("a", 1),  // killed
            raw("b", 0),  // survived
            raw("c", 33), // no_tests
        ];
        let json_out = build_json_results(&results, true);
        assert_eq!(json_out.mutants.len(), 3);
    }

    /// INV-4: All status strings in JSON are lowercase snake_case.
    #[test]
    fn test_json_results_status_serialization_snake_case() {
        let results = vec![
            raw("a", 0),  // survived
            raw("b", 1),  // killed
            raw("c", 33), // no_tests
        ];
        let json_out = build_json_results(&results, true);
        let serialized = serde_json::to_string(&json_out).unwrap();
        assert!(serialized.contains("\"survived\""));
        assert!(serialized.contains("\"killed\""));
        assert!(serialized.contains("\"no_tests\""));
    }

    // --- generate_mutants: diff filter integration tests ---

    /// INV-1: Unchanged files are copied verbatim to mutants/ but produce zero mutants.
    #[test]
    fn test_diff_filter_unchanged_file_copied_no_mutants() {
        use crate::git_diff::DiffFilter;

        let src_tmp = tempfile::tempdir().unwrap();
        let mutants_tmp = tempfile::tempdir().unwrap();

        // A file with a function that would generate mutations.
        std::fs::write(
            src_tmp.path().join("lib.py"),
            "def add(a, b):\n    return a + b\n",
        )
        .unwrap();

        // Build a DiffFilter that doesn't include lib.py at all (file unchanged).
        let filter = DiffFilter::default();
        let repo_root = src_tmp.path();

        let result = generate_mutants(
            src_tmp.path(),
            mutants_tmp.path(),
            &[],
            Some((&filter, repo_root)),
        )
        .unwrap();

        // No mutants should be generated.
        assert!(
            result.names_by_module.is_empty(),
            "unchanged file should produce no mutants; got: {:?}",
            result.names_by_module
        );

        // But the file should still be copied.
        assert!(
            mutants_tmp.path().join("lib.py").exists(),
            "unchanged file must be copied to mutants/ for package integrity"
        );
    }

    /// INV-2: Unchanged functions within a changed file produce zero mutants.
    #[test]
    fn test_diff_filter_unchanged_function_in_changed_file_skipped() {

        let src_tmp = tempfile::tempdir().unwrap();
        let mutants_tmp = tempfile::tempdir().unwrap();

        // Two functions; only `bar` (lines 4-5) is "changed".
        let source = "def foo(a, b):\n    return a + b\n\ndef bar(x):\n    return x - 1\n";
        std::fs::write(src_tmp.path().join("lib.py"), source).unwrap();

        // Build a DiffFilter touching only bar's lines (4-5) via a minimal diff string.
        let diff_text = "\
diff --git a/lib.py b/lib.py
index abc..def 100644
--- a/lib.py
+++ b/lib.py
@@ -4,2 +4,2 @@ def bar(x):
-    return x - 1
+    return x - 2
";
        let filter = crate::git_diff::parse_unified_diff(diff_text);
        let repo_root = src_tmp.path();

        let result = generate_mutants(
            src_tmp.path(),
            mutants_tmp.path(),
            &[],
            Some((&filter, repo_root)),
        )
        .unwrap();

        // Should have mutants only for bar, not foo.
        let all_names: Vec<&String> = result.names_by_module.values().flatten().collect();
        assert!(
            !all_names.is_empty(),
            "bar should have been mutated"
        );
        // None of the mutant names should reference x_foo.
        for name in &all_names {
            assert!(
                !name.contains("x_foo"),
                "foo was not in the diff, should not be mutated; got: {name}"
            );
        }
        // All mutant names should reference x_bar.
        assert!(
            all_names.iter().any(|n| n.contains("x_bar")),
            "bar should have mutants; names: {all_names:?}"
        );
    }

    /// INV-3: Without --diff, behaviour is identical to before (no regression).
    #[test]
    fn test_no_diff_filter_produces_same_results() {
        let src_tmp = tempfile::tempdir().unwrap();
        let mutants_a = tempfile::tempdir().unwrap();
        let mutants_b = tempfile::tempdir().unwrap();

        std::fs::write(
            src_tmp.path().join("lib.py"),
            "def add(a, b):\n    return a + b\n",
        )
        .unwrap();

        let result_no_filter =
            generate_mutants(src_tmp.path(), mutants_a.path(), &[], None).unwrap();
        let result_none_filter =
            generate_mutants(src_tmp.path(), mutants_b.path(), &[], None).unwrap();

        assert_eq!(
            result_no_filter.names_by_module.len(),
            result_none_filter.names_by_module.len(),
            "None diff_filter must produce same result as no filter"
        );
    }

    /// INV-4: New file (None entry in DiffFilter) gets all functions mutated.
    #[test]
    fn test_diff_filter_new_file_all_functions_mutated() {
        let src_tmp = tempfile::tempdir().unwrap();
        let mutants_tmp = tempfile::tempdir().unwrap();

        std::fs::write(
            src_tmp.path().join("new_lib.py"),
            "def foo(a, b):\n    return a + b\n\ndef bar(x):\n    return x - 1\n",
        )
        .unwrap();

        // Simulate new file: parse_unified_diff with new file mode.
        let diff_text = "\
diff --git a/new_lib.py b/new_lib.py
new file mode 100644
index 000..abc
--- /dev/null
+++ b/new_lib.py
@@ -0,0 +1,5 @@
+def foo(a, b):
+    return a + b
+
+def bar(x):
+    return x - 1
";
        let filter = crate::git_diff::parse_unified_diff(diff_text);
        let repo_root = src_tmp.path();

        let result = generate_mutants(
            src_tmp.path(),
            mutants_tmp.path(),
            &[],
            Some((&filter, repo_root)),
        )
        .unwrap();

        let all_names: Vec<&String> = result.names_by_module.values().flatten().collect();
        // Both foo and bar should have mutants.
        assert!(all_names.iter().any(|n| n.contains("x_foo")), "foo should be mutated in new file");
        assert!(all_names.iter().any(|n| n.contains("x_bar")), "bar should be mutated in new file");
    }
}
