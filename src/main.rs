use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "irradiate", about = "Mutation testing for Python", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run mutation testing
    Run {
        /// Specific mutant names to test (default: all)
        mutant_names: Vec<String>,

        /// Path(s) to source code to mutate (default: "src", overrides pyproject.toml)
        #[arg(long)]
        paths_to_mutate: Option<String>,

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

        /// Respawn workers after N mutants to prevent pytest state accumulation (0 to disable)
        #[arg(long, default_value_t = 100)]
        worker_recycle_after: usize,

        /// Recycle workers whose RSS exceeds this threshold in megabytes (0 to disable)
        #[arg(long, default_value_t = 0)]
        max_worker_memory: usize,

        /// Run each mutant in a fresh subprocess (slower, better isolation)
        #[arg(long)]
        isolate: bool,
    },

    /// Display mutation testing results
    Results {
        /// Show all mutants, not just survived
        #[arg(long)]
        all: bool,
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
        } => {
            // Load pyproject.toml config; CLI flags override config values.
            let file_config = irradiate::config::load_config(&std::env::current_dir()?)?;

            irradiate::pipeline::run(irradiate::pipeline::RunConfig {
                paths_to_mutate: PathBuf::from(
                    paths_to_mutate
                        .or(file_config.paths_to_mutate)
                        .unwrap_or_else(|| "src".to_string()),
                ),
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
                do_not_mutate: file_config.do_not_mutate.unwrap_or_default(),
            })
            .await
        }
        Commands::Results { all } => irradiate::pipeline::results(all),
        Commands::Show { mutant_name } => irradiate::pipeline::show(&mutant_name),
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
