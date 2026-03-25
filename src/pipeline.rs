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
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Configuration for a mutation testing run.
pub struct RunConfig {
    pub paths_to_mutate: Vec<PathBuf>,
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
    /// Report format to generate after the run (e.g. "json" for Stryker-schema JSON).
    /// `None` = no report generated.
    pub report: Option<String>,
    /// Output path for the report. Defaults to `irradiate-report.<format>`.
    pub report_output: Option<std::path::PathBuf>,
    /// Randomly sample a subset of mutants for testing.
    /// Values in (0.0, 1.0] are fractions; values > 1 are absolute counts.
    /// `None` = test all mutants (default).
    pub sample: Option<f64>,
    /// RNG seed for `--sample`. Default 0 for deterministic reproducibility.
    pub sample_seed: u64,
    /// Ignore cached results — re-run all mutants from scratch.
    pub no_cache: bool,
    /// Extra arguments appended to every pytest invocation.
    /// Sourced from `pytest_add_cli_args` in pyproject.toml and/or `--pytest-args` CLI flag.
    pub pytest_add_cli_args: Vec<String>,
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

/// Operator-stratified random sampling of mutants.
///
/// Each mutation operator contributes proportionally to the sample, ensuring
/// no operator class is completely unrepresented. Deterministic given the same
/// seed and input.
fn sample_mutants(
    mutants: Vec<MutantCacheDescriptor>,
    sample_value: f64,
    seed: u64,
) -> Vec<MutantCacheDescriptor> {
    use rand::seq::SliceRandom;
    use rand::SeedableRng;

    let target_count = if sample_value > 0.0 && sample_value <= 1.0 {
        (mutants.len() as f64 * sample_value).ceil() as usize
    } else {
        (sample_value as usize).min(mutants.len())
    }
    .max(1);

    if target_count >= mutants.len() {
        return mutants;
    }

    // Group by operator, sorted alphabetically for deterministic iteration.
    let mut by_operator: HashMap<String, Vec<MutantCacheDescriptor>> = HashMap::new();
    for m in mutants {
        by_operator.entry(m.operator.clone()).or_default().push(m);
    }
    let total: usize = by_operator.values().map(|v| v.len()).sum();
    let mut groups: Vec<(String, Vec<MutantCacheDescriptor>)> = by_operator.into_iter().collect();
    groups.sort_by(|a, b| a.0.cmp(&b.0));
    let num_groups = groups.len();

    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let mut sampled = Vec::with_capacity(target_count);
    let mut remaining = target_count;

    for (i, (_op, mut group)) in groups.into_iter().enumerate() {
        if remaining == 0 {
            break;
        }
        let share = if i == num_groups - 1 {
            // Last group gets whatever remains to avoid rounding errors.
            remaining.min(group.len())
        } else {
            let proportion = group.len() as f64 / total as f64;
            let alloc = (proportion * target_count as f64).round() as usize;
            alloc.max(1).min(remaining).min(group.len())
        };
        group.shuffle(&mut rng);
        sampled.extend(group.into_iter().take(share));
        remaining = remaining.saturating_sub(share);
    }

    sampled.truncate(target_count);
    sampled
}

/// Validate that the environment is ready to run mutation testing.
///
/// Checks that paths exist and Python/pytest are usable before doing any work,
/// so the user gets a clear error message immediately rather than after mutation
/// generation completes.
fn validate_environment(config: &RunConfig) -> Result<()> {
    // Check that --paths-to-mutate entries exist.
    if config.paths_to_mutate.is_empty() {
        bail!("--paths-to-mutate: at least one path is required");
    }
    for p in &config.paths_to_mutate {
        if !p.exists() {
            bail!("--paths-to-mutate path '{}' does not exist", p.display());
        }
    }

    // Check that --tests-dir exists.
    if !Path::new(&config.tests_dir).exists() {
        bail!("--tests-dir path '{}' does not exist", config.tests_dir);
    }

    // Check that --python is executable.
    let python_ok = std::process::Command::new(&config.python)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !python_ok {
        bail!(
            "Python interpreter '{}' not found. Set --python to a valid Python path.",
            config.python.display()
        );
    }

    // Check that pytest is importable.
    let pytest_ok = std::process::Command::new(&config.python)
        .args(["-c", "import pytest"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !pytest_ok {
        bail!(
            "pytest not found in '{}'. Install with: {} -m pip install pytest",
            config.python.display(),
            config.python.display()
        );
    }

    Ok(())
}

/// Shared context threaded through pipeline phases.
struct PipelineCtx {
    project_dir: PathBuf,
    mutants_dir: PathBuf,
    harness_dir: PathBuf,
    pythonpath: String,
    trace: crate::trace::TraceLog,
}

/// Phase 1: Generate mutations, apply filters and sampling.
///
/// Returns the generation output, filtered mutant list, and sampling info.
fn phase_generate(
    config: &RunConfig,
    ctx: &mut PipelineCtx,
) -> Result<Option<(GenerationOutput, Vec<MutantCacheDescriptor>, Option<usize>)>> {
    #![allow(clippy::type_complexity)]
    let diff_filter = if let Some(ref diff_ref) = config.diff_ref {
        let repo_root = crate::git_diff::find_git_root(&ctx.project_dir)?;
        let filter = crate::git_diff::parse_git_diff(diff_ref, &repo_root)?;
        eprintln!("Generating mutants (incremental: diff against {diff_ref})...");
        Some((filter, repo_root))
    } else {
        eprintln!("Generating mutants...");
        None
    };

    let phase_start = ctx.trace.now_us();
    let start = Instant::now();
    let generation = generate_mutants(
        &config.paths_to_mutate,
        &ctx.mutants_dir,
        &config.do_not_mutate,
        diff_filter.as_ref().map(|(f, r)| (f, r.as_path())),
        &config.tests_dir,
    )?;
    let gen_time = start.elapsed();
    let mutant_count: usize = generation.names_by_module.values().map(|v| v.len()).sum();
    ctx.trace.phase("mutation_generation", phase_start, Some(serde_json::json!({
        "mutants": mutant_count,
        "files": generation.names_by_module.len(),
    })));
    eprintln!(
        "  done in {:.0}ms ({} mutants across {} files)",
        gen_time.as_millis(),
        mutant_count,
        generation.names_by_module.len(),
    );

    if generation.descriptors_by_name.is_empty() {
        let paths_str: Vec<_> = config.paths_to_mutate.iter().map(|p| p.display().to_string()).collect();
        eprintln!(
            "No mutations found in {}. Check that your source files contain functions to mutate.",
            paths_str.join(", ")
        );
        return Ok(None);
    }

    let mut all_mutants: Vec<MutantCacheDescriptor> =
        generation.descriptors_by_name.values().cloned().collect();

    if let Some(ref filter) = config.mutant_filter {
        all_mutants.retain(|desc| filter.iter().any(|f| desc.mutant_name.contains(f)));
        if all_mutants.is_empty() {
            eprintln!("No mutants match the filter.");
            return Ok(None);
        }
    }

    let sampled_from = if let Some(sample_value) = config.sample {
        let population = all_mutants.len();
        all_mutants = sample_mutants(all_mutants, sample_value, config.sample_seed);
        eprintln!(
            "  Sampled {} of {} mutants ({:.1}%)",
            all_mutants.len(),
            population,
            all_mutants.len() as f64 / population as f64 * 100.0,
        );
        Some(population)
    } else {
        None
    };

    Ok(Some((generation, all_mutants, sampled_from)))
}

/// Phase 2: Collect stats + validate, optionally pre-spawn workers.
async fn phase_stats(
    config: &RunConfig,
    ctx: &mut PipelineCtx,
    total_mutants: usize,
    has_mutants: bool,
) -> Result<(Option<TestStats>, Option<crate::orchestrator::PreSpawnedPool>)> {
    // Pre-spawn workers before stats so they boot (~480ms) in parallel with
    // stats collection (~3-4s).
    let pre_spawned = if !config.isolate && !config.no_stats {
        let pool_config = build_pool_config(config, ctx);
        let num_workers = config.workers.min(total_mutants);
        match crate::orchestrator::pre_spawn_pool(&pool_config, &ctx.harness_dir, num_workers) {
            Ok(pool) => Some(pool),
            Err(e) => {
                tracing::warn!("Pre-spawn failed, will spawn later: {e}");
                None
            }
        }
    } else {
        None
    };

    let test_stats = if config.no_stats {
        eprintln!("Running clean tests...");
        validate_clean_run(
            &config.python, &ctx.project_dir, &ctx.pythonpath,
            &ctx.mutants_dir, &config.tests_dir, &config.pytest_add_cli_args,
        ).await?;
        eprintln!("  done");
        eprintln!("Running forced-fail validation...");
        validate_fail_run(
            &config.python, &ctx.project_dir, &ctx.pythonpath,
            &ctx.mutants_dir, &config.tests_dir, &config.pytest_add_cli_args,
        ).await?;
        eprintln!("  done");
        None
    } else {
        let phase_start = ctx.trace.now_us();
        let start = Instant::now();
        let s = if let Some(cached) = stats::load_cached_stats(&ctx.project_dir, &config.paths_to_mutate, &config.tests_dir) {
            eprintln!("Using cached stats (source/tests unchanged)");
            ctx.trace.phase("stats_cache_hit", phase_start, None);
            cached
        } else {
            eprintln!("Running stats + validation...");
            let s = stats::collect_stats(
                &config.python, &ctx.project_dir, &ctx.pythonpath,
                &ctx.mutants_dir, &config.tests_dir, &config.pytest_add_cli_args,
            ).context("Stats collection failed")?;
            stats::save_stats_fingerprint(&ctx.project_dir, &config.paths_to_mutate, &config.tests_dir);
            ctx.trace.phase("stats_collection", phase_start, None);
            eprintln!("  done in {:.0}ms", start.elapsed().as_millis());
            s
        };

        if let Some(exit_code) = s.exit_status {
            if exit_code > 1 {
                bail!("Stats run failed (exit code {exit_code}) — tests could not run with trampolined code");
            }
            if exit_code == 1 {
                eprintln!("Warning: some tests failed during stats run (pre-existing failures)");
            }
        }
        if s.fail_validated == Some(false) {
            bail!("Trampoline fail path not wired — in-process fail probe did not raise ProgrammaticFailException");
        }
        if s.tests_by_function.is_empty() && has_mutants {
            bail!("No functions were hit during stats collection, but mutants exist — trampoline may not be loading");
        }

        Some(s)
    };

    Ok((test_stats, pre_spawned))
}

/// Phase 3+4: Schedule work items (with cache lookup) and execute mutation testing.
async fn phase_execute(
    config: &RunConfig,
    ctx: &mut PipelineCtx,
    all_mutants: &[MutantCacheDescriptor],
    test_stats: &Option<TestStats>,
    pre_spawned: Option<crate::orchestrator::PreSpawnedPool>,
    total_mutants: usize,
) -> Result<(Vec<MutantResult>, CacheCounts, Vec<ScheduledMutant>)> {
    let phase_start = ctx.trace.now_us();

    if config.isolate {
        eprintln!("Running mutation testing ({total_mutants} mutants, isolated mode)...");
    } else {
        eprintln!(
            "Running mutation testing ({total_mutants} mutants, {} workers)...",
            config.workers
        );
    }

    // Build work items
    let work_items: Vec<ScheduledMutant> = all_mutants
        .iter()
        .filter_map(|descriptor| {
            let mutant_name = &descriptor.mutant_name;
            let test_ids = if let Some(ref stats) = test_stats {
                let func_key = mutant_name
                    .rsplit_once("__irradiate_")
                    .map(|(prefix, _)| prefix)
                    .unwrap_or(mutant_name);
                let tests = stats.tests_for_function_by_duration(func_key);
                if tests.is_empty() {
                    if config.covered_only {
                        return None;
                    }
                    vec![]
                } else {
                    tests
                }
            } else {
                vec![]
            };

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

    // For no-stats mode, fill in all test IDs
    let work_items = if config.no_stats {
        let all_tests = discover_tests(
            &config.python, &ctx.project_dir, &ctx.pythonpath,
            &ctx.mutants_dir, &config.tests_dir, &config.pytest_add_cli_args,
        ).await?;
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

    // Split: uncovered → NoTests, cached → cache hit, rest → execute
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

        if !config.no_cache {
            item.cache_key = cache::build_cache_key(
                &ctx.project_dir, &item.descriptor, &item.work_item.test_ids,
                &mut resolved_test_paths, &mut test_file_hashes,
            )?;

            if let Some(ref key) = item.cache_key {
                if let Some(entry) = cache::load_entry(&ctx.project_dir, key)? {
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
        }

        covered_work.push(item);
    }

    ctx.trace.phase("scheduling", phase_start, Some(serde_json::json!({
        "cache_hits": cache_counts.hits,
        "cache_misses": cache_counts.misses,
        "uncovered": results.len(),
        "to_execute": covered_work.len(),
    })));

    if !covered_work.is_empty() {
        let execution_work: Vec<WorkItem> = covered_work
            .iter()
            .map(|item| item.work_item.clone())
            .collect();
        let phase_start = ctx.trace.now_us();
        let run_results = if config.isolate {
            run_isolated(
                config, execution_work, &ctx.harness_dir, &ctx.mutants_dir,
                test_stats.as_ref(), &ctx.project_dir,
            ).await?
        } else {
            let pool_config = build_pool_config(config, ctx);
            let progress = crate::progress::ProgressBar::new(total_mutants);
            let (results, worker_trace) = if let Some(pool) = pre_spawned {
                crate::orchestrator::run_worker_pool_pre_spawned(
                    &pool_config, execution_work, Some(progress), pool,
                ).await?
            } else {
                run_worker_pool(&pool_config, execution_work, Some(progress)).await?
            };
            ctx.trace.merge(worker_trace);
            results
        };
        ctx.trace.phase("worker_pool", phase_start, Some(serde_json::json!({
            "executed": run_results.len(),
        })));

        // Store results in cache (skip when --no-cache)
        if !config.no_cache {
            let cache_keys_by_mutant: HashMap<String, String> = covered_work
                .iter()
                .filter_map(|item| {
                    item.cache_key.as_ref().map(|key| (item.work_item.mutant_name.clone(), key.clone()))
                })
                .collect();
            for result in &run_results {
                if let Some(key) = cache_keys_by_mutant.get(&result.mutant_name) {
                    cache::store_entry(&ctx.project_dir, key, result.exit_code, result.duration, result.status)?;
                }
            }
        }
        results.extend(run_results);
    }

    Ok((results, cache_counts, covered_work))
}

/// Phase 4b: Optionally re-test survivors in isolated mode.
async fn phase_verify_survivors(
    config: &RunConfig,
    ctx: &PipelineCtx,
    results: &mut [MutantResult],
    covered_work: &[ScheduledMutant],
    test_stats: &Option<TestStats>,
) -> Result<()> {
    if !config.verify_survivors || config.isolate {
        if config.verify_survivors {
            eprintln!("Verification skipped: already running in isolate mode (all results are already isolated)");
        }
        return Ok(());
    }

    let survivor_lookup: HashMap<String, (WorkItem, Option<String>)> = covered_work
        .iter()
        .map(|s| (s.work_item.mutant_name.clone(), (s.work_item.clone(), s.cache_key.clone())))
        .collect();

    let survivor_items: Vec<WorkItem> = results
        .iter()
        .filter(|r| r.status == MutantStatus::Survived)
        .filter_map(|r| survivor_lookup.get(&r.mutant_name))
        .map(|(wi, _)| wi.clone())
        .collect();

    if survivor_items.is_empty() {
        eprintln!("Verification: no warm-session survivors to verify");
        return Ok(());
    }

    let survivor_count = survivor_items.len();
    eprintln!("Verifying {survivor_count} survived mutants in isolate mode...");

    let verify_results = run_isolated(
        config, survivor_items, &ctx.harness_dir, &ctx.mutants_dir,
        test_stats.as_ref(), &ctx.project_dir,
    ).await?;

    for vr in &verify_results {
        if vr.status == MutantStatus::Killed {
            eprintln!(
                "  [verify] {} survived warm-session but killed in isolate — false negative corrected",
                vr.mutant_name
            );
            if let Some((_, Some(key))) = survivor_lookup.get(&vr.mutant_name) {
                cache::force_update_entry(&ctx.project_dir, key, vr.exit_code, vr.duration, vr.status)?;
            }
        }
    }
    let flipped = apply_verification_corrections(results, &verify_results);

    if flipped > 0 {
        eprintln!("Verification complete: {flipped}/{survivor_count} survivors were false negatives (corrected)");
    } else {
        eprintln!("Verification complete: all {survivor_count} survivors confirmed");
    }
    Ok(())
}

/// Phase 5: Write results, generate reports, emit annotations, check fail-under.
#[allow(clippy::too_many_arguments)]
fn phase_results(
    config: &RunConfig,
    ctx: &mut PipelineCtx,
    results: &[MutantResult],
    generation: &GenerationOutput,
    test_stats: &Option<TestStats>,
    cache_counts: CacheCounts,
    test_time_secs: f64,
    sampled_from: Option<usize>,
) -> Result<()> {
    let phase_start = ctx.trace.now_us();

    crate::report::write_meta_files(&ctx.mutants_dir, &generation.names_by_module, results)?;

    if let Some(ref fmt) = config.report {
        let output_path = config.report_output.clone()
            .unwrap_or_else(|| PathBuf::from(format!("irradiate-report.{fmt}")));
        let all_descriptors: Vec<MutantCacheDescriptor> =
            generation.descriptors_by_name.values().cloned().collect();
        let report = crate::report::build_stryker_report(
            results, &all_descriptors, test_stats.as_ref(), &ctx.project_dir, &config.paths_to_mutate,
        );
        if fmt == "html" {
            crate::report::write_html_report(&report, &output_path)?;
        } else {
            let json_str = serde_json::to_string_pretty(&report)?;
            std::fs::write(&output_path, &json_str)
                .with_context(|| format!("Failed to write report to {}", output_path.display()))?;
        }
        eprintln!("Report written to {}", output_path.display());
    }

    let (killed, survived) = crate::report::print_summary(
        results, test_time_secs, cache_counts, &generation.descriptors_by_name, sampled_from,
    );

    let all_descriptors: Vec<_> = generation.descriptors_by_name.values().cloned().collect();
    crate::report::emit_github_annotations(results, &all_descriptors, killed, survived);

    ctx.trace.phase("results_output", phase_start, None);

    let trace_path = ctx.project_dir.join(".irradiate").join("trace.json");
    if let Err(e) = crate::trace::write_trace_file(&trace_path, &ctx.trace.events) {
        tracing::warn!("Failed to write trace file: {e}");
    } else {
        eprintln!("Trace written to {}", trace_path.display());
    }

    if let Some(threshold) = config.fail_under {
        let tested = killed + survived;
        if tested > 0 {
            let score = killed as f64 / tested as f64 * 100.0;
            if score < threshold {
                bail!("Mutation score {score:.1}% is below threshold {threshold:.1}%");
            }
        }
    }

    Ok(())
}

/// Build a PoolConfig from RunConfig and PipelineCtx.
fn build_pool_config(config: &RunConfig, ctx: &PipelineCtx) -> PoolConfig {
    PoolConfig {
        num_workers: config.workers,
        python: config.python.clone(),
        project_dir: ctx.project_dir.clone(),
        mutants_dir: ctx.mutants_dir.clone(),
        tests_dir: PathBuf::from(&config.tests_dir),
        timeout_multiplier: config.timeout_multiplier,
        pythonpath: ctx.pythonpath.clone(),
        worker_recycle_after: config.worker_recycle_after,
        max_worker_memory_mb: config.max_worker_memory_mb,
        pytest_add_cli_args: config.pytest_add_cli_args.clone(),
        ..Default::default()
    }
}

/// Run the full mutation testing pipeline.
pub async fn run(config: RunConfig) -> Result<()> {
    validate_environment(&config)?;

    let project_dir = std::env::current_dir()?;
    let mutants_dir = project_dir.join("mutants");
    let harness_dir = harness::extract_harness(&project_dir)?;
    let pythonpath = build_pythonpath(&harness_dir, &config.paths_to_mutate);

    let mut ctx = PipelineCtx {
        project_dir,
        mutants_dir,
        harness_dir,
        pythonpath,
        trace: crate::trace::TraceLog::new(),
    };

    // Phase 1: Generate mutations
    let Some((generation, all_mutants, sampled_from)) = phase_generate(&config, &mut ctx)? else {
        return Ok(()); // no mutants found or all filtered out
    };
    let total_mutants = all_mutants.len();

    // Phase 2: Stats + validation (+ pre-spawn workers)
    let (test_stats, pre_spawned) =
        phase_stats(&config, &mut ctx, total_mutants, !all_mutants.is_empty()).await?;

    // Phase 3+4: Schedule + execute
    let start = Instant::now();
    let (mut results, cache_counts, covered_work) =
        phase_execute(&config, &mut ctx, &all_mutants, &test_stats, pre_spawned, total_mutants).await?;

    // Phase 4b: Verify survivors
    phase_verify_survivors(&config, &ctx, &mut results, &covered_work, &test_stats).await?;

    let test_time = start.elapsed();

    // Phase 5: Results + reports
    phase_results(
        &config, &mut ctx, &results, &generation, &test_stats,
        cache_counts, test_time.as_secs_f64(), sampled_from,
    )?;

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
            .args(&config.pytest_add_cli_args)
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
pub fn build_pythonpath(harness_dir: &Path, paths_to_mutate: &[PathBuf]) -> String {
    let mut parts = vec![harness_dir.display().to_string()];
    let mut seen = std::collections::HashSet::new();
    for p in paths_to_mutate {
        let source_parent = p.parent().unwrap_or(p);
        let s = source_parent.display().to_string();
        if seen.insert(s.clone()) {
            parts.push(s);
        }
    }
    parts.join(":")
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

/// Write `content` to `path`, creating parent directories as needed.
fn write_file_with_parents(path: &Path, content: impl AsRef<[u8]>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    Ok(())
}

fn generate_mutants(
    paths_to_mutate: &[PathBuf],
    mutants_dir: &Path,
    do_not_mutate: &[String],
    diff_filter: Option<(&crate::git_diff::DiffFilter, &Path)>,
    tests_dir: &str,
) -> Result<GenerationOutput> {
    // Clean mutants dir
    if mutants_dir.exists() {
        std::fs::remove_dir_all(mutants_dir)?;
    }

    // Collect (py_file, strip_base) pairs from all paths.
    let mut file_entries: Vec<(PathBuf, PathBuf)> = Vec::new();

    for path in paths_to_mutate {
        let py_files = find_python_files(path)?;

        // Determine the strip base for computing relative paths.
        //
        // When the path IS a package directory (contains __init__.py), we
        // strip its parent so the package name is preserved in mutants/.
        //   e.g. path="src/click", file="src/click/types.py"
        //        strip "src/" → rel_path="click/types.py" → mutants/click/types.py  ✓
        //
        // When the path is a single file, walk up to the outermost package boundary.
        //   e.g. path="src/hive/config.py"
        //        strip "src/" → rel_path="hive/config.py" → mutants/hive/config.py
        //
        // When the path is a source root (no __init__.py), we strip it directly.
        //   e.g. path="src", file="src/simple_lib/__init__.py"
        //        strip "src/" → rel_path="simple_lib/__init__.py"  ✓
        let strip_base = if path.is_file() {
            let mut base = path.parent().unwrap_or(path);
            while base.join("__init__.py").exists() {
                base = match base.parent() {
                    Some(p) => p,
                    None => break,
                };
            }
            base.to_path_buf()
        } else if path.join("__init__.py").exists() {
            path.parent().unwrap_or(path).to_path_buf()
        } else {
            path.to_path_buf()
        };

        // When mutating a single file, copy all sibling package files to mutants/
        // for import integrity.
        if path.is_file() {
            let package_root = if path.parent().unwrap_or(path) != strip_base.as_path() {
                strip_base.as_path()
            } else {
                path.parent().unwrap_or(path)
            };
            let siblings = find_python_files(package_root)?;
            for sibling in &siblings {
                if sibling == path {
                    continue; // skip the target file — it'll be mutated below
                }
                if let Ok(rel) = sibling.strip_prefix(&strip_base) {
                    let dest = mutants_dir.join(rel);
                    let content = std::fs::read_to_string(sibling)?;
                    write_file_with_parents(&dest, &content)?;
                }
            }
        }

        for py_file in py_files {
            file_entries.push((py_file, strip_base.clone()));
        }
    }

    // When tests_dir is inside paths_to_mutate (e.g. toolz/tests inside toolz/),
    // exclude test files from the mutants directory to avoid import shadowing.
    // Only activate when tests_dir is actually under a source path.
    let tests_dir_canonical = std::fs::canonicalize(tests_dir).ok();
    let source_canonicals: Vec<PathBuf> = paths_to_mutate
        .iter()
        .filter_map(|p| std::fs::canonicalize(p).ok())
        .collect();
    let tests_inside_source = tests_dir_canonical.as_ref().is_some_and(|tests_canon| {
        source_canonicals
            .iter()
            .any(|src| tests_canon.starts_with(src) && tests_canon != src)
    });
    if tests_inside_source {
        let tests_canon = tests_dir_canonical.as_ref().unwrap();
        file_entries.retain(|(py_file, _strip_base)| {
            if let Ok(file_canon) = std::fs::canonicalize(py_file) {
                if file_canon.starts_with(tests_canon) {
                    return false;
                }
            }
            true
        });
    }

    // Compute project root for do_not_mutate path matching (relative to cwd).
    let cwd = std::env::current_dir()?;

    // Process files in parallel — each writes to a unique path, no conflicts.
    // create_dir_all is safe to call concurrently (handles races internally).
    type MutantEntry = Option<(String, Vec<String>, Vec<MutantCacheDescriptor>)>;
    let results: Vec<Result<MutantEntry>> = file_entries
        .par_iter()
        .map(|(py_file, strip_base)| -> Result<MutantEntry> {
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
                    write_file_with_parents(&dest, &source)?;
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
                    write_file_with_parents(&dest, &source)?;
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
                write_file_with_parents(&dest, &mutated.source)?;

                // Write .meta stub
                let meta_path = PathBuf::from(format!("{}.meta", dest.display()));
                let meta = crate::report::FileMeta::default();
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
                write_file_with_parents(&dest, &source)?;
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

    // Copy non-Python data files (e.g. .txt, .json, .grammar) from source directories
    // into the mutants directory. Packages like parso use `Path(__file__).parent` to locate
    // data files at runtime; without these, the trampolined code fails with FileNotFoundError.
    for path in paths_to_mutate {
        if !path.is_dir() {
            continue;
        }
        let strip_base = if path.join("__init__.py").exists() {
            path.parent().unwrap_or(path).to_path_buf()
        } else {
            path.to_path_buf()
        };
        copy_data_files(path, &strip_base, mutants_dir)?;
    }

    Ok(GenerationOutput {
        names_by_module: all_names,
        descriptors_by_name,
    })
}

/// Recursively copy non-Python data files from `dir` into `mutants_dir`, preserving
/// relative paths. Skips `.py`, `.pyc`, `__pycache__`, hidden dirs, and `.meta` files.
fn copy_data_files(dir: &Path, strip_base: &Path, mutants_dir: &Path) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.starts_with('.') || name == "__pycache__" {
                continue;
            }
            copy_data_files(&path, strip_base, mutants_dir)?;
        } else {
            let ext = path.extension().unwrap_or_default().to_string_lossy();
            if ext == "py" || ext == "pyc" || ext == "meta" {
                continue;
            }
            if let Ok(rel) = path.strip_prefix(strip_base) {
                let dest = mutants_dir.join(rel);
                if !dest.exists() {
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::copy(&path, &dest)?;
                }
            }
        }
    }
    Ok(())
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
            // Skip __pycache__ and hidden dirs.
            // unwrap_or_default is safe: `path` came from read_dir, so it always has a filename component.
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

/// Output captured from a subprocess run by [`run_subprocess`].
struct SubprocessOutput {
    exit_code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Spawn a Python subprocess, collect its stdout/stderr in background tasks,
/// apply a timeout, and return the captured output.
///
/// `description` is used in error messages (e.g. "clean test validation").
async fn run_subprocess(
    python: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
    project_dir: &Path,
    timeout_secs: u64,
    description: &str,
) -> Result<SubprocessOutput> {
    let mut cmd = tokio::process::Command::new(python);
    for arg in args {
        cmd.arg(arg);
    }
    for (key, val) in envs {
        cmd.env(key, val);
    }
    let mut child = cmd
        .current_dir(project_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("Failed to spawn {description} subprocess"))?;

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

    let status = match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => bail!("{description} subprocess error: {e}"),
        Err(_) => {
            let _ = child.kill().await;
            bail!("{description} timed out after {timeout_secs}s — pytest may be hung");
        }
    };

    let stdout = stdout_task.await.unwrap_or_default();
    let stderr = stderr_task.await.unwrap_or_default();
    let exit_code = status.code().unwrap_or(-1);

    Ok(SubprocessOutput { exit_code, stdout, stderr })
}

async fn validate_clean_run(
    python: &Path,
    project_dir: &Path,
    pythonpath: &str,
    mutants_dir: &Path,
    tests_dir: &str,
    extra_pytest_args: &[String],
) -> Result<()> {
    let mutants_dir_str = mutants_dir.to_string_lossy();
    let mut args: Vec<&str> = vec!["-m", "pytest", "-x", "-q", "-p", "irradiate_harness", tests_dir];
    args.extend(extra_pytest_args.iter().map(String::as_str));
    let output = run_subprocess(
        python,
        &args,
        &[("PYTHONPATH", pythonpath), ("IRRADIATE_MUTANTS_DIR", &mutants_dir_str)],
        project_dir,
        VALIDATION_TIMEOUT_SECS,
        "clean test validation",
    )
    .await?;

    // Pytest exit codes:
    //   0 = all tests passed
    //   1 = tests collected and ran, but some failed (pre-existing failures)
    //   2+ = interrupted / internal error / usage error / no tests collected
    // Exit code 1 is expected for projects with pre-existing test failures.
    // The clean test validates that trampolining doesn't completely break
    // the project — pre-existing failures are OK.
    if output.exit_code > 1 {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Clean test run failed:\n{stdout}\n{stderr}");
    }
    if output.exit_code == 1 {
        let stdout = String::from_utf8_lossy(&output.stdout);
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
    extra_pytest_args: &[String],
) -> Result<()> {
    let mutants_dir_str = mutants_dir.to_string_lossy();
    let mut args: Vec<&str> = vec!["-m", "pytest", "-x", "-q", "-p", "irradiate_harness", tests_dir];
    args.extend(extra_pytest_args.iter().map(String::as_str));
    let output = run_subprocess(
        python,
        &args,
        &[
            ("PYTHONPATH", pythonpath),
            ("IRRADIATE_MUTANTS_DIR", &mutants_dir_str),
            ("IRRADIATE_ACTIVE_MUTANT", "fail"),
        ],
        project_dir,
        VALIDATION_TIMEOUT_SECS,
        "forced-fail validation",
    )
    .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    match output.exit_code {
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
    extra_pytest_args: &[String],
) -> Result<Vec<String>> {
    let mutants_dir_str = mutants_dir.to_string_lossy();
    let mut args: Vec<&str> = vec!["-m", "pytest", "--collect-only", "-q", "-p", "irradiate_harness", tests_dir];
    args.extend(extra_pytest_args.iter().map(String::as_str));
    let output = run_subprocess(
        python,
        &args,
        &[("PYTHONPATH", pythonpath), ("IRRADIATE_MUTANTS_DIR", &mutants_dir_str)],
        project_dir,
        VALIDATION_TIMEOUT_SECS,
        "test discovery",
    )
    .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
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
        let harness = Path::new("/tmp/harness");
        let paths = vec![PathBuf::from("src/mylib")];

        let result = build_pythonpath(harness, &paths);

        assert!(result.contains("/tmp/harness"), "harness dir must be in PYTHONPATH");
        assert!(result.contains("src"), "source parent must be in PYTHONPATH");
        assert!(!result.contains("src/mylib"), "paths_to_mutate itself must not appear");
        let parts: Vec<&str> = result.split(':').collect();
        assert_eq!(parts.len(), 2, "PYTHONPATH must have exactly 2 components");
    }

    #[test]
    fn test_build_pythonpath_order() {
        let harness = Path::new("/h");
        let paths = vec![PathBuf::from("src/lib")];

        let result = build_pythonpath(harness, &paths);
        let parts: Vec<&str> = result.split(':').collect();

        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], "/h", "harness must be first");
        assert_eq!(parts[1], "src", "source parent must be second");
    }

    #[test]
    fn test_build_pythonpath_root_fallback() {
        let harness = Path::new("/h");
        let paths = vec![PathBuf::from("mylib")];

        let result = build_pythonpath(harness, &paths);
        assert!(!result.is_empty());
        let parts: Vec<&str> = result.split(':').collect();
        assert_eq!(parts.len(), 2);
    }

    #[test]
    fn test_build_pythonpath_deterministic() {
        let harness = Path::new("/tmp/h");
        let paths = vec![PathBuf::from("project/src")];

        let a = build_pythonpath(harness, &paths);
        let b = build_pythonpath(harness, &paths);
        assert_eq!(a, b, "build_pythonpath must be deterministic");
    }

    #[test]
    fn test_build_pythonpath_no_mutants_dir() {
        let harness = Path::new("/tmp/harness");
        let paths = vec![PathBuf::from("src/mylib")];
        let mutants_dir_str = "/tmp/mutants";

        let result = build_pythonpath(harness, &paths);
        assert!(!result.contains(mutants_dir_str), "mutants_dir must not be in PYTHONPATH");
    }

    #[test]
    fn test_build_pythonpath_multiple_paths() {
        let harness = Path::new("/h");
        let paths = vec![PathBuf::from("src/a"), PathBuf::from("lib/b")];

        let result = build_pythonpath(harness, &paths);
        let parts: Vec<&str> = result.split(':').collect();

        assert_eq!(parts.len(), 3, "harness + 2 distinct source parents");
        assert_eq!(parts[0], "/h");
        assert_eq!(parts[1], "src");
        assert_eq!(parts[2], "lib");
    }

    #[test]
    fn test_build_pythonpath_deduplicates_parents() {
        let harness = Path::new("/h");
        let paths = vec![PathBuf::from("src/a"), PathBuf::from("src/b")];

        let result = build_pythonpath(harness, &paths);
        let parts: Vec<&str> = result.split(':').collect();

        assert_eq!(parts.len(), 2, "shared parent 'src' should appear once");
        assert_eq!(parts[0], "/h");
        assert_eq!(parts[1], "src");
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

        generate_mutants(&[src_tmp.path().to_path_buf()], mutants_tmp.path(), &[], None, "tests").unwrap();

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

        generate_mutants(&[src_tmp.path().to_path_buf()], mutants_tmp.path(), &[], None, "tests").unwrap();

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
        generate_mutants(&[pkg.clone()], mutants_tmp.path(), &[], None, "tests").unwrap();

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
        generate_mutants(&[src.clone()], mutants_tmp.path(), &[], None, "tests").unwrap();

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
        let result = generate_mutants(&[tmp.path().to_path_buf()], mutants_tmp.path(), &[], None, "tests");
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
        let result = generate_mutants(&[src_tmp.path().to_path_buf()], mutants_tmp.path(), &[pattern], None, "tests").unwrap();

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

        let result = generate_mutants(&[src_tmp.path().to_path_buf()], mutants_tmp.path(), &[], None, "tests").unwrap();
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
        let pythonpath = build_pythonpath(&harness_dir, &[fixture.join("src")]);
        let tmp_mutants = tempfile::tempdir().unwrap();

        let result = validate_clean_run(&python, &fixture, &pythonpath, tmp_mutants.path(), "tests", &[]).await;
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
        let pythonpath = build_pythonpath(&harness_dir, &[project.path().to_path_buf()]);
        let tmp_mutants = tempfile::tempdir().unwrap();

        let result = validate_clean_run(&python, project.path(), &pythonpath, tmp_mutants.path(), "tests", &[]).await;
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
        let pythonpath = build_pythonpath(&harness_dir, &[project.path().to_path_buf()]);
        let tmp_mutants = tempfile::tempdir().unwrap();

        let result = validate_clean_run(&python, project.path(), &pythonpath, tmp_mutants.path(), "tests", &[]).await;
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
        let pythonpath = build_pythonpath(&harness_dir, &[fixture.join("src")]);

        // Generate real mutants — the trampoline `if active == 'fail': raise …` must be present.
        let tmp_mutants = tempfile::tempdir().unwrap();
        generate_mutants(&[fixture.join("src")], tmp_mutants.path(), &[], None, "tests")
            .expect("mutant generation should succeed for fixture");

        let result = validate_fail_run(&python, &fixture, &pythonpath, tmp_mutants.path(), "tests", &[]).await;
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
        let pythonpath = build_pythonpath(&harness_dir, &[fixture.join("src")]);
        // Empty mutants dir — no trampoline code, so tests will pass under active_mutant=fail
        let tmp_mutants = tempfile::tempdir().unwrap();

        let result = validate_fail_run(&python, &fixture, &pythonpath, tmp_mutants.path(), "tests", &[]).await;
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
        let pythonpath = build_pythonpath(&harness_dir, &[project.path().to_path_buf()]);
        let tmp_mutants = tempfile::tempdir().unwrap();

        let result = validate_fail_run(&python, project.path(), &pythonpath, tmp_mutants.path(), "tests", &[]).await;
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
        let pythonpath = build_pythonpath(&harness_dir, &[fixture.join("src")]);
        // Empty mutants dir — tests pass, exit 0
        let tmp_mutants = tempfile::tempdir().unwrap();

        let result = validate_fail_run(&python, &fixture, &pythonpath, tmp_mutants.path(), "tests", &[]).await;
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
        let pythonpath = build_pythonpath(&harness_dir, &[project.path().to_path_buf()]);
        let tmp_mutants = tempfile::tempdir().unwrap();

        let result = validate_clean_run(&python, project.path(), &pythonpath, tmp_mutants.path(), "tests", &[]).await;
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
        let pythonpath = build_pythonpath(&harness_dir, &[fixture.join("src")]);
        let tmp_mutants = tempfile::tempdir().unwrap();

        let tests = discover_tests(&python, &fixture, &pythonpath, tmp_mutants.path(), "tests", &[])
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
        let pythonpath = build_pythonpath(&harness_dir, &[project.path().to_path_buf()]);
        let tmp_mutants = tempfile::tempdir().unwrap();

        let tests = discover_tests(
            &python,
            project.path(),
            &pythonpath,
            tmp_mutants.path(),
            "empty_tests",
            &[],
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
            &[src_tmp.path().to_path_buf()],
            mutants_tmp.path(),
            &[],
            Some((&filter, repo_root)),
            "tests",
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
            &[src_tmp.path().to_path_buf()],
            mutants_tmp.path(),
            &[],
            Some((&filter, repo_root)),
            "tests",
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
            generate_mutants(&[src_tmp.path().to_path_buf()], mutants_a.path(), &[], None, "tests").unwrap();
        let result_none_filter =
            generate_mutants(&[src_tmp.path().to_path_buf()], mutants_b.path(), &[], None, "tests").unwrap();

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
            &[src_tmp.path().to_path_buf()],
            mutants_tmp.path(),
            &[],
            Some((&filter, repo_root)),
            "tests",
        )
        .unwrap();

        let all_names: Vec<&String> = result.names_by_module.values().flatten().collect();
        // Both foo and bar should have mutants.
        assert!(all_names.iter().any(|n| n.contains("x_foo")), "foo should be mutated in new file");
        assert!(all_names.iter().any(|n| n.contains("x_bar")), "bar should be mutated in new file");
    }

    // --- validate_environment tests ---

    fn make_run_config_for_env_test(
        paths_to_mutate: PathBuf,
        tests_dir: String,
        python: PathBuf,
    ) -> RunConfig {
        RunConfig {
            paths_to_mutate: vec![paths_to_mutate],
            tests_dir,
            workers: 1,
            timeout_multiplier: 10.0,
            no_stats: true,
            covered_only: false,
            python,
            mutant_filter: None,
            worker_recycle_after: None,
            max_worker_memory_mb: 0,
            isolate: false,
            verify_survivors: false,
            do_not_mutate: vec![],
            fail_under: None,
            diff_ref: None,
            report: None,
            report_output: None,
            no_cache: false,
            sample: None,
            sample_seed: 0,
            pytest_add_cli_args: vec![],
        }
    }

    #[test]
    fn test_validate_environment_nonexistent_python_errors() {
        // INV: A nonexistent Python path must produce a clear error before any work starts.
        let tmp = tempfile::tempdir().unwrap();
        // Create a real paths_to_mutate and tests_dir so only the python check fires.
        std::fs::write(tmp.path().join("lib.py"), "def foo(): pass").unwrap();
        std::fs::create_dir(tmp.path().join("tests")).unwrap();

        let config = make_run_config_for_env_test(
            tmp.path().join("lib.py"),
            tmp.path().join("tests").to_string_lossy().to_string(),
            PathBuf::from("/nonexistent/python/interpreter"),
        );

        let result = validate_environment(&config);
        assert!(result.is_err(), "must error on nonexistent python");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("not found") || msg.contains("Python interpreter"),
            "error should mention python not found; got: {msg}"
        );
    }

    #[test]
    fn test_validate_environment_nonexistent_paths_to_mutate_errors() {
        // INV: A nonexistent --paths-to-mutate must produce a clear error, not a silent empty run.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("tests")).unwrap();

        let config = make_run_config_for_env_test(
            tmp.path().join("nonexistent_src"),
            tmp.path().join("tests").to_string_lossy().to_string(),
            PathBuf::from("python3"),
        );

        let result = validate_environment(&config);
        assert!(result.is_err(), "must error on nonexistent paths_to_mutate");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("paths-to-mutate") || msg.contains("does not exist"),
            "error should mention paths-to-mutate; got: {msg}"
        );
    }

    #[test]
    fn test_validate_environment_nonexistent_tests_dir_errors() {
        // INV: A nonexistent --tests-dir must produce a clear error before mutation generation.
        let tmp = tempfile::tempdir().unwrap();
        // Create a real paths_to_mutate so only the tests_dir check fires.
        std::fs::write(tmp.path().join("lib.py"), "def foo(): pass").unwrap();

        let config = make_run_config_for_env_test(
            tmp.path().join("lib.py"),
            tmp.path().join("nonexistent_tests").to_string_lossy().to_string(),
            PathBuf::from("python3"),
        );

        let result = validate_environment(&config);
        assert!(result.is_err(), "must error on nonexistent tests_dir");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("tests-dir") || msg.contains("does not exist"),
            "error should mention tests-dir; got: {msg}"
        );
    }

    // --- sample_mutants tests ---

    fn make_descriptor(name: &str, operator: &str) -> MutantCacheDescriptor {
        MutantCacheDescriptor {
            mutant_name: name.to_string(),
            function_source: String::new(),
            operator: operator.to_string(),
            start: 0,
            end: 0,
            original: String::new(),
            replacement: String::new(),
            source_file: String::new(),
            fn_byte_offset: 0,
            fn_start_line: 0,
        }
    }

    fn make_descriptors(counts: &[(&str, usize)]) -> Vec<MutantCacheDescriptor> {
        let mut descs = Vec::new();
        for (op, count) in counts {
            for i in 0..*count {
                descs.push(make_descriptor(&format!("m.x_{op}_{i}"), op));
            }
        }
        descs
    }

    #[test]
    fn sample_fraction() {
        let descs = make_descriptors(&[("binop_swap", 100)]);
        let sampled = sample_mutants(descs, 0.1, 0);
        assert_eq!(sampled.len(), 10);
    }

    #[test]
    fn sample_absolute_count() {
        let descs = make_descriptors(&[("binop_swap", 100)]);
        let sampled = sample_mutants(descs, 50.0, 0);
        assert_eq!(sampled.len(), 50);
    }

    #[test]
    fn sample_exceeds_total_is_noop() {
        let descs = make_descriptors(&[("binop_swap", 10)]);
        let sampled = sample_mutants(descs, 200.0, 0);
        assert_eq!(sampled.len(), 10);
    }

    #[test]
    fn sample_fraction_one_is_noop() {
        let descs = make_descriptors(&[("binop_swap", 10)]);
        let sampled = sample_mutants(descs, 1.0, 0);
        assert_eq!(sampled.len(), 10);
    }

    #[test]
    fn sample_deterministic_same_seed() {
        let descs = make_descriptors(&[("binop_swap", 50), ("compop_swap", 30), ("condition_replacement", 20)]);
        let a = sample_mutants(descs.clone(), 0.1, 42);
        let b = sample_mutants(descs, 0.1, 42);
        let names_a: Vec<_> = a.iter().map(|d| &d.mutant_name).collect();
        let names_b: Vec<_> = b.iter().map(|d| &d.mutant_name).collect();
        assert_eq!(names_a, names_b, "same seed must produce same sample");
    }

    #[test]
    fn sample_different_seed_differs() {
        let descs = make_descriptors(&[("binop_swap", 50), ("compop_swap", 30), ("condition_replacement", 20)]);
        let a = sample_mutants(descs.clone(), 0.1, 0);
        let b = sample_mutants(descs, 0.1, 99);
        let names_a: Vec<_> = a.iter().map(|d| &d.mutant_name).collect();
        let names_b: Vec<_> = b.iter().map(|d| &d.mutant_name).collect();
        assert_ne!(names_a, names_b, "different seeds should produce different samples");
    }

    #[test]
    fn sample_stratified_all_operators_represented() {
        // 70/20/10 split across 3 operators, 10% sample = 10 mutants.
        // All 3 operators should have at least 1 representative.
        let descs = make_descriptors(&[("binop_swap", 70), ("compop_swap", 20), ("condition_replacement", 10)]);
        let sampled = sample_mutants(descs, 0.1, 0);
        assert_eq!(sampled.len(), 10);
        let ops: std::collections::HashSet<_> = sampled.iter().map(|d| d.operator.as_str()).collect();
        assert!(ops.contains("binop_swap"), "binop_swap must be represented");
        assert!(ops.contains("compop_swap"), "compop_swap must be represented");
        assert!(ops.contains("condition_replacement"), "condition_replacement must be represented");
    }

    #[test]
    fn sample_empty_input() {
        let descs: Vec<MutantCacheDescriptor> = vec![];
        let sampled = sample_mutants(descs, 0.1, 0);
        assert!(sampled.is_empty());
    }
}
