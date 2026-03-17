//! Load mutation testing configuration from pyproject.toml `[tool.mutmut]` section.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Default, Deserialize)]
pub struct ProjectConfig {
    #[serde(default)]
    pub tool: ToolConfig,
}

#[derive(Debug, Default, Deserialize)]
pub struct ToolConfig {
    #[serde(default)]
    pub mutmut: MutmutConfig,
}

#[derive(Debug, Default, Deserialize)]
pub struct MutmutConfig {
    pub paths_to_mutate: Option<String>,
    pub tests_dir: Option<String>,
    pub do_not_mutate: Option<Vec<String>>,
    pub also_copy: Option<Vec<String>>,
    pub debug: Option<bool>,
    pub pytest_add_cli_args: Option<String>,
}

/// Load `[tool.mutmut]` from pyproject.toml in `project_dir`.
/// Returns default (all-None) config if the file is absent.
pub fn load_config(project_dir: &Path) -> Result<MutmutConfig> {
    let pyproject = project_dir.join("pyproject.toml");
    if !pyproject.exists() {
        return Ok(MutmutConfig::default());
    }
    let content = std::fs::read_to_string(&pyproject).context("Failed to read pyproject.toml")?;
    let config: ProjectConfig =
        toml::from_str(&content).context("Failed to parse pyproject.toml")?;
    Ok(config.tool.mutmut)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_full_config() {
        let toml_str = r#"
[tool.mutmut]
paths_to_mutate = "lib"
tests_dir = "test"
do_not_mutate = ["vendor/*"]
debug = true
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.tool.mutmut.paths_to_mutate.as_deref(), Some("lib"));
        assert_eq!(config.tool.mutmut.tests_dir.as_deref(), Some("test"));
        assert!(config.tool.mutmut.debug.unwrap());
        let do_not_mutate = config.tool.mutmut.do_not_mutate.as_ref().unwrap();
        assert_eq!(do_not_mutate.len(), 1);
        assert_eq!(do_not_mutate[0], "vendor/*");
    }

    #[test]
    fn test_missing_mutmut_section() {
        let toml_str = r#"
[tool.ruff]
line-length = 144
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert!(config.tool.mutmut.paths_to_mutate.is_none());
        assert!(config.tool.mutmut.tests_dir.is_none());
        assert!(config.tool.mutmut.debug.is_none());
    }

    #[test]
    fn test_empty_file() {
        let config: ProjectConfig = toml::from_str("").unwrap();
        assert!(config.tool.mutmut.paths_to_mutate.is_none());
        assert!(config.tool.mutmut.tests_dir.is_none());
    }

    #[test]
    fn test_unknown_keys_ignored() {
        // INV-4: Unknown keys in [tool.mutmut] must not cause parse failure
        let toml_str = r#"
[tool.mutmut]
paths_to_mutate = "src"
future_unknown_key = "something"
"#;
        // serde default behavior: unknown fields are ignored
        let result: Result<ProjectConfig, _> = toml::from_str(toml_str);
        assert!(result.is_ok());
        let config = result.unwrap();
        assert_eq!(config.tool.mutmut.paths_to_mutate.as_deref(), Some("src"));
    }

    #[test]
    fn test_load_config_missing_file() {
        // INV-2: Missing pyproject.toml is not an error
        let tmp = tempfile::tempdir().unwrap();
        let result = load_config(tmp.path());
        assert!(result.is_ok());
        let cfg = result.unwrap();
        assert!(cfg.paths_to_mutate.is_none());
    }

    #[test]
    fn test_load_config_invalid_toml() {
        // INV-3: Invalid TOML produces clear error
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("pyproject.toml"), "[[[ invalid").unwrap();
        let result = load_config(tmp.path());
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("pyproject.toml"), "error message should mention pyproject.toml: {msg}");
    }

    #[test]
    fn test_all_supported_keys() {
        let toml_str = r#"
[tool.mutmut]
paths_to_mutate = "src"
tests_dir = "tests"
do_not_mutate = ["src/generated/*", "src/vendor/*"]
also_copy = ["data/"]
debug = false
pytest_add_cli_args = "-v --tb=short"
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        let m = &config.tool.mutmut;
        assert_eq!(m.paths_to_mutate.as_deref(), Some("src"));
        assert_eq!(m.tests_dir.as_deref(), Some("tests"));
        assert_eq!(m.do_not_mutate.as_ref().unwrap().len(), 2);
        assert_eq!(m.also_copy.as_ref().unwrap(), &["data/"]);
        assert_eq!(m.debug, Some(false));
        assert_eq!(m.pytest_add_cli_args.as_deref(), Some("-v --tb=short"));
    }
}
