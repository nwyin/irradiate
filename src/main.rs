use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;


/// Resolve the Python interpreter to use.
///
/// Priority:
/// 1. Sibling of the current executable (e.g. `.venv/bin/irradiate` -> `.venv/bin/python3`)
/// 2. `VIRTUAL_ENV` env var -> `$VIRTUAL_ENV/bin/python3`
/// 3. Bare `python3` (PATH lookup)
fn resolve_python() -> PathBuf {
    // Check sibling of our own binary — this is the most reliable signal when
    // installed into a venv via `uv pip install` / `pip install`.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(bin_dir) = exe.parent() {
            let sibling = bin_dir.join("python3");
            if sibling.exists() {
                return sibling;
            }
        }
    }

    // Fall back to VIRTUAL_ENV env var (set by activate scripts).
    if let Ok(venv) = std::env::var("VIRTUAL_ENV") {
        let venv_python = PathBuf::from(&venv).join("bin").join("python3");
        if venv_python.exists() {
            return venv_python;
        }
    }

    // Last resort: bare name, rely on PATH.
    PathBuf::from("python3")
}

fn parse_sample(s: &str) -> std::result::Result<f64, String> {
    let v: f64 = s
        .parse()
        .map_err(|_| format!("'{s}' is not a valid number"))?;
    if v <= 0.0 {
        return Err("sample must be positive".to_string());
    }
    Ok(v)
}

fn parse_fail_under(s: &str) -> std::result::Result<f64, String> {
    let v: f64 = s
        .parse()
        .map_err(|_| format!("'{s}' is not a valid number"))?;
    if !(0.0..=100.0).contains(&v) {
        return Err(format!("value must be between 0.0 and 100.0, got {v}"));
    }
    Ok(v)
}

#[derive(Parser)]
#[command(name = "irradiate", about = "Mutation testing for Python", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum Commands {
    /// Run mutation testing
    Run {
        /// Path(s) to source code to mutate (default: "src", overrides pyproject.toml).
        /// Example: irradiate run src/mylib
        paths: Vec<String>,

        /// Filter to specific mutant names (advanced — test only these mutants)
        #[arg(long)]
        mutant: Vec<String>,

        /// Alias for positional paths, for backward compatibility.
        /// Prefer positional: `irradiate run src/` instead of `--paths-to-mutate src/`
        #[arg(long)]
        paths_to_mutate: Vec<String>,

        /// Path to test directory (default: "tests", overrides pyproject.toml)
        #[arg(long)]
        tests_dir: Option<String>,

        /// Number of worker processes
        #[arg(long)]
        workers: Option<usize>,

        /// Timeout multiplier (applied to baseline test duration)
        #[arg(long, default_value_t = 10.0)]
        timeout_multiplier: f64,

        /// Skip stats collection, test all mutants against all tests
        #[arg(long)]
        no_stats: bool,

        /// Skip mutants with no test coverage
        #[arg(long)]
        covered_only: bool,

        /// Python interpreter path (auto-detected from venv if not set)
        #[arg(long)]
        python: Option<String>,

        /// Recycle workers whose RSS exceeds this threshold in megabytes.
        /// Default: 1024 on macOS, 0 (unlimited) on Linux. Pass 0 to disable.
        #[arg(long)]
        max_worker_memory: Option<usize>,

        /// Disable fork-per-mutant in workers (default on macOS to avoid kernel panics
        /// from memory pressure). Slightly less isolation but avoids fork() overhead.
        #[arg(long)]
        no_fork: bool,

        /// Force fork-per-mutant even on macOS (overrides --no-fork and the macOS default).
        #[arg(long)]
        fork: bool,

        /// Run each mutant in a fresh subprocess (slower, better isolation)
        #[arg(long)]
        isolate: bool,

        /// After the main run, re-test survived mutants in isolate mode to detect
        /// false negatives from warm-session state leakage. No-op when --isolate is set.
        #[arg(long)]
        verify_survivors: bool,

        /// Fail with exit code 1 if mutation score (killed/tested*100) is below this threshold.
        /// Value must be between 0.0 and 100.0. Only applied when at least one mutant is tested.
        #[arg(long, value_parser = parse_fail_under)]
        fail_under: Option<f64>,

        /// Only mutate functions changed since this git ref (e.g., main, HEAD~3).
        /// Requires a git repository.
        #[arg(long)]
        diff: Option<String>,

        /// Generate a report in the specified format (json = Stryker schema v2)
        #[arg(long)]
        report: Option<String>,

        /// Output path for the generated report (default: irradiate-report.<format>)
        #[arg(short = 'o', long)]
        output: Option<PathBuf>,

        /// Randomly sample a subset of mutants. Values 0.0-1.0 are fractions (e.g. 0.1 = 10%).
        /// Values > 1 are absolute counts (e.g. 100 = test exactly 100 mutants).
        #[arg(long, value_parser = parse_sample)]
        sample: Option<f64>,

        /// RNG seed for --sample (default: 0 for reproducibility)
        #[arg(long, default_value_t = 0)]
        sample_seed: u64,

        /// Ignore cached results — re-test all mutants from scratch
        #[arg(long)]
        no_cache: bool,

        /// Timeout in seconds for stats collection (default: 300).
        /// Increase for large test suites (e.g., 600 for 10K+ tests).
        #[arg(long, default_value_t = 300)]
        stats_timeout: u64,

        /// Extra arguments to pass to every pytest invocation (appended after config file values).
        /// Example: --pytest-args=-v --pytest-args=--tb=short
        #[arg(long = "pytest-args")]
        pytest_args: Vec<String>,

        /// Timeout in seconds for workers to complete test collection (default: 30).
        /// Increase for projects with slow imports (e.g. --worker-timeout 120 for tinygrad/torch).
        #[arg(long, default_value_t = 30)]
        worker_timeout: u64,

        /// Shell command to run before the mutation testing run (e.g. download remote cache).
        /// Overrides cache_pre_sync in pyproject.toml.
        #[arg(long)]
        cache_pre_sync: Option<String>,

        /// Shell command to run after the mutation testing run (e.g. upload cache to remote).
        /// Overrides cache_post_sync in pyproject.toml.
        #[arg(long)]
        cache_post_sync: Option<String>,

        /// Run a type checker to filter mutants caught by static analysis.
        /// Accepts a preset name (mypy, pyright, ty) or a raw command string.
        /// Mutants that introduce type errors are marked as killed (exit code 37).
        #[arg(long)]
        type_checker: Option<String>,

        /// Disable source-patch mutations for decorated functions.
        /// Only trampoline-compatible functions (@property, @classmethod, @staticmethod)
        /// will be mutated. Skips the slower source-patch phase.
        #[arg(long)]
        no_source_patch: bool,

        /// Glob patterns for files to exclude from mutation. Can be repeated.
        /// Merged with do_not_mutate from pyproject.toml.
        #[arg(long)]
        ignore: Vec<String>,

        /// Only run these mutation operators (allowlist). Supports glob patterns.
        /// Can be repeated. Mutually exclusive with --skip-operators.
        #[arg(long = "operators")]
        operators: Vec<String>,

        /// Skip these mutation operators (denylist). Supports glob patterns (e.g., regex_*).
        /// Can be repeated. Mutually exclusive with --operators.
        #[arg(long = "skip-operators")]
        skip_operators: Vec<String>,
    },

    /// Display mutation testing results
    Results {
        /// Show all mutants, not just survived
        #[arg(long)]
        all: bool,

        /// Output machine-readable JSON instead of text
        #[arg(long)]
        json: bool,

        /// Generate a report in the specified format (json = Stryker schema v2)
        #[arg(long)]
        report: Option<String>,

        /// Output path for the generated report (default: irradiate-report.<format>)
        #[arg(short = 'o', long)]
        output: Option<PathBuf>,
    },

    /// Show diff for a specific mutant
    Show {
        /// Mutant name (e.g., module.x_func__irradiate_1)
        mutant_name: String,
    },

    /// Manage local cache state
    Cache {
        #[command(subcommand)]
        command: CacheCommands,
    },
}

#[derive(Subcommand)]
enum CacheCommands {
    /// Remove the local cache directory
    Clean,
    /// Garbage-collect old or excess cache entries
    Gc {
        /// Maximum age for cache entries (e.g. "30d", "24h", "1h30m"). Default: "30d".
        /// Overrides cache_max_age in pyproject.toml.
        #[arg(long, default_value = None)]
        max_age: Option<String>,

        /// Maximum total cache size (e.g. "1gb", "500mb"). Default: "1gb".
        /// Overrides cache_max_size in pyproject.toml.
        #[arg(long, default_value = None)]
        max_size: Option<String>,

        /// Show what would be pruned without deleting anything
        #[arg(long)]
        dry_run: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Run {
            paths,
            mutant,
            paths_to_mutate,
            tests_dir,
            workers,
            timeout_multiplier,
            no_stats,
            covered_only,
            python,
            max_worker_memory,
            isolate,
            verify_survivors,
            fail_under,
            diff,
            report,
            output,
            no_cache,
            stats_timeout,
            sample,
            sample_seed,
            pytest_args,
            worker_timeout,
            cache_pre_sync,
            cache_post_sync,
            type_checker,
            no_source_patch,
            no_fork,
            fork,
            ignore,
            operators,
            skip_operators,
        } => {
            // Load pyproject.toml config; CLI flags override config values.
            let file_config = irradiate::config::load_config(&std::env::current_dir()?)?;

            // pytest_add_cli_args: start from config, then append CLI --pytest-args
            let mut pytest_add_cli_args = file_config.pytest_add_cli_args.unwrap_or_default();
            pytest_add_cli_args.extend(pytest_args);

            // do_not_mutate: merge config with CLI --ignore patterns
            let mut do_not_mutate = file_config.do_not_mutate.unwrap_or_default();
            do_not_mutate.extend(ignore);

            // Merge positional paths and --paths-to-mutate (positional takes priority)
            let mut all_paths = paths;
            all_paths.extend(paths_to_mutate);

            // Operator filter: CLI flags override config; allow and deny are mutually exclusive.
            let op_allow = if !operators.is_empty() { Some(operators) } else { file_config.operators };
            let op_deny = if !skip_operators.is_empty() { Some(skip_operators) } else { file_config.skip_operators };
            let operator_filter = match (op_allow, op_deny) {
                (Some(_), Some(_)) => anyhow::bail!(
                    "--operators and --skip-operators are mutually exclusive. \
                     Use one or the other (check pyproject.toml too)."
                ),
                (Some(ops), None) => Some(irradiate::pipeline::OperatorFilter::Allow(ops)),
                (None, Some(ops)) => Some(irradiate::pipeline::OperatorFilter::Deny(ops)),
                (None, None) => None,
            };

            irradiate::pipeline::run(irradiate::pipeline::RunConfig {
                paths_to_mutate: {
                    let raw = if !all_paths.is_empty() {
                        all_paths
                    } else {
                        file_config
                            .paths_to_mutate
                            .unwrap_or_else(|| vec!["src".to_string()])
                    };
                    raw.into_iter().map(PathBuf::from).collect()
                },
                tests_dir: tests_dir
                    .or(file_config.tests_dir)
                    .unwrap_or_else(|| "tests".to_string()),
                workers: workers.or(file_config.workers).unwrap_or_else(num_cpus::get),
                timeout_multiplier,
                no_stats,
                covered_only,
                python: python.map(PathBuf::from).unwrap_or_else(resolve_python),
                mutant_filter: if mutant.is_empty() {
                    None
                } else {
                    Some(mutant)
                },
                max_worker_memory_mb: max_worker_memory
                    .or(file_config.max_worker_memory_mb)
                    .unwrap_or(if cfg!(target_os = "macos") { 1024 } else { 0 }),
                isolate,
                verify_survivors,
                do_not_mutate,
                fail_under,
                diff_ref: diff,
                report,
                report_output: output,
                no_cache,
                sample,
                sample_seed,
                stats_timeout,
                pytest_add_cli_args,
                worker_ready_timeout: worker_timeout,
                cache_pre_sync: cache_pre_sync.or(file_config.cache_pre_sync),
                cache_post_sync: cache_post_sync.or(file_config.cache_post_sync),
                type_checker: type_checker.or(file_config.type_checker),
                no_source_patch,
                no_fork: if fork {
                    false
                } else if no_fork {
                    true
                } else {
                    // Default to no-fork on macOS to avoid kernel panics from
                    // fork() memory pressure on machines with limited RAM.
                    cfg!(target_os = "macos")
                },
                operator_filter,
            })
            .await
        }
        Commands::Results { all, json, report, output } => {
            irradiate::report::results(all, json, report, output)
        }
        Commands::Show { mutant_name } => irradiate::report::show(&mutant_name),
        Commands::Cache { command } => match command {
            CacheCommands::Clean => {
                let project_dir = std::env::current_dir()?;
                if irradiate::cache::clean(&project_dir)? {
                    eprintln!(
                        "Removed local cache at {}",
                        irradiate::cache::cache_dir(&project_dir).display()
                    );
                } else {
                    eprintln!(
                        "No local cache found at {}",
                        irradiate::cache::cache_dir(&project_dir).display()
                    );
                }
                Ok(())
            }
            CacheCommands::Gc { max_age, max_size, dry_run } => {
                let project_dir = std::env::current_dir()?;
                let file_config = irradiate::config::load_config(&project_dir)?;

                let age_str = max_age
                    .or(file_config.cache_max_age)
                    .unwrap_or_else(|| "30d".to_string());
                let size_str = max_size
                    .or(file_config.cache_max_size)
                    .unwrap_or_else(|| "1gb".to_string());

                let max_age_secs = irradiate::cache::parse_duration(&age_str)?;
                let max_size_bytes = irradiate::cache::parse_size(&size_str)?;

                let result = irradiate::cache::gc(&project_dir, max_age_secs, max_size_bytes, dry_run)?;

                fn fmt_size(bytes: u64) -> String {
                    if bytes >= 1024 * 1024 * 1024 {
                        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
                    } else if bytes >= 1024 * 1024 {
                        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
                    } else if bytes >= 1024 {
                        format!("{:.1} KB", bytes as f64 / 1024.0)
                    } else {
                        format!("{bytes} B")
                    }
                }

                if result.pruned == 0 {
                    eprintln!(
                        "Cache is within limits. {} entries ({}).",
                        result.remaining,
                        fmt_size(result.remaining_bytes),
                    );
                } else if dry_run {
                    eprintln!(
                        "Would prune {} entries ({}). Cache: {} entries ({} total).",
                        result.pruned,
                        fmt_size(result.pruned_bytes),
                        result.remaining + result.pruned,
                        fmt_size(result.remaining_bytes + result.pruned_bytes),
                    );
                } else {
                    eprintln!(
                        "Pruned {} entries ({} freed). Cache: {} entries ({} remaining).",
                        result.pruned,
                        fmt_size(result.pruned_bytes),
                        result.remaining,
                        fmt_size(result.remaining_bytes),
                    );
                }
                Ok(())
            }
        },
    }
}
