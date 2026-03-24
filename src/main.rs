use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

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
        /// Specific mutant names to test (default: all)
        mutant_names: Vec<String>,

        /// Path(s) to source code to mutate (default: "src", overrides pyproject.toml).
        /// Can be specified multiple times: --paths-to-mutate src/a.py --paths-to-mutate src/b.py
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

        /// Python interpreter path
        #[arg(long, default_value = "python3")]
        python: String,

        /// Respawn workers after N mutants to prevent pytest state accumulation (0 to disable).
        /// Default: auto-tune (100 normally, reduced to 20 when session-scoped fixtures detected).
        #[arg(long)]
        worker_recycle_after: Option<usize>,

        /// Recycle workers whose RSS exceeds this threshold in megabytes (0 to disable)
        #[arg(long, default_value_t = 0)]
        max_worker_memory: usize,

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

        /// Extra arguments to pass to every pytest invocation (appended after config file values).
        /// Example: --pytest-args=-v --pytest-args=--tb=short
        #[arg(long = "pytest-args")]
        pytest_args: Vec<String>,
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
            mutant_names,
            paths_to_mutate,
            tests_dir,
            workers,
            timeout_multiplier,
            no_stats,
            covered_only,
            python,
            worker_recycle_after,
            max_worker_memory,
            isolate,
            verify_survivors,
            fail_under,
            diff,
            report,
            output,
            sample,
            sample_seed,
            pytest_args,
        } => {
            // Load pyproject.toml config; CLI flags override config values.
            let file_config = irradiate::config::load_config(&std::env::current_dir()?)?;

            // pytest_add_cli_args: start from config, then append CLI --pytest-args
            let mut pytest_add_cli_args = file_config.pytest_add_cli_args.unwrap_or_default();
            pytest_add_cli_args.extend(pytest_args);

            irradiate::pipeline::run(irradiate::pipeline::RunConfig {
                paths_to_mutate: {
                    let raw = if !paths_to_mutate.is_empty() {
                        paths_to_mutate
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
                workers: workers.unwrap_or_else(num_cpus::get),
                timeout_multiplier,
                no_stats,
                covered_only,
                python: PathBuf::from(python),
                mutant_filter: if mutant_names.is_empty() {
                    None
                } else {
                    Some(mutant_names)
                },
                worker_recycle_after,
                max_worker_memory_mb: max_worker_memory,
                isolate,
                verify_survivors,
                do_not_mutate: file_config.do_not_mutate.unwrap_or_default(),
                fail_under,
                diff_ref: diff,
                report,
                report_output: output,
                sample,
                sample_seed,
                pytest_add_cli_args,
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
        },
    }
}
