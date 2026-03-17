use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

const HARNESS_INIT: &str = include_str!("../harness/__init__.py");
const HARNESS_WORKER: &str = include_str!("../harness/worker.py");
const HARNESS_STATS_PLUGIN: &str = include_str!("../harness/stats_plugin.py");

/// Extract the embedded Python harness files to the given directory.
/// Returns the path to the harness directory.
pub fn extract_harness(base_dir: &Path) -> Result<PathBuf> {
    let harness_dir = base_dir.join(".irradiate").join("harness");
    fs::create_dir_all(&harness_dir).context("Failed to create harness directory")?;

    // Write the irradiate_harness package
    let pkg_dir = harness_dir.join("irradiate_harness");
    fs::create_dir_all(&pkg_dir).context("Failed to create irradiate_harness package directory")?;

    fs::write(pkg_dir.join("__init__.py"), HARNESS_INIT).context("Failed to write __init__.py")?;
    fs::write(harness_dir.join("worker.py"), HARNESS_WORKER)
        .context("Failed to write worker.py")?;
    fs::write(pkg_dir.join("stats_plugin.py"), HARNESS_STATS_PLUGIN)
        .context("Failed to write stats_plugin.py")?;

    Ok(harness_dir)
}

/// Get the path to the worker.py script within the harness directory.
pub fn worker_script(harness_dir: &Path) -> PathBuf {
    harness_dir.join("worker.py")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn test_extract_harness() {
        let tmp = env::temp_dir().join("irradiate_test_harness");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        let harness_dir = extract_harness(&tmp).unwrap();

        assert!(harness_dir
            .join("irradiate_harness")
            .join("__init__.py")
            .exists());
        assert!(harness_dir.join("worker.py").exists());
        assert!(harness_dir
            .join("irradiate_harness")
            .join("stats_plugin.py")
            .exists());

        // Verify content
        let init_content =
            fs::read_to_string(harness_dir.join("irradiate_harness").join("__init__.py")).unwrap();
        assert!(init_content.contains("active_mutant"));
        assert!(init_content.contains("ProgrammaticFailException"));

        let worker_content = fs::read_to_string(harness_dir.join("worker.py")).unwrap();
        assert!(worker_content.contains("IRRADIATE_SOCKET"));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_worker_script_path() {
        let harness_dir = Path::new("/tmp/harness");
        assert_eq!(
            worker_script(harness_dir),
            PathBuf::from("/tmp/harness/worker.py")
        );
    }
}
