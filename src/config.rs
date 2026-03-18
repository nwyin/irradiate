//! Load mutation testing configuration from pyproject.toml `[tool.irradiate]` section.
//! Falls back to `[tool.mutmut]` for migration compatibility.

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
    pub irradiate: Option<IrradiateConfig>,
    #[serde(default)]
    pub mutmut: Option<IrradiateConfig>,
}

#[derive(Debug, Default, Deserialize)]
pub struct IrradiateConfig {
    pub paths_to_mutate: Option<String>,
    pub tests_dir: Option<String>,
    pub do_not_mutate: Option<Vec<String>>,
    pub also_copy: Option<Vec<String>>,
    pub debug: Option<bool>,
    pub pytest_add_cli_args: Option<String>,
}

/// Load `[tool.irradiate]` from pyproject.toml in `project_dir`.
/// Falls back to `[tool.mutmut]` for backward compatibility, with a deprecation warning.
/// Returns default (all-None) config if the file is absent or neither section exists.
pub fn load_config(project_dir: &Path) -> Result<IrradiateConfig> {
    let pyproject = project_dir.join("pyproject.toml");
    if !pyproject.exists() {
        return Ok(IrradiateConfig::default());
    }
    let content = std::fs::read_to_string(&pyproject).context("Failed to read pyproject.toml")?;
    let config: ProjectConfig =
        toml::from_str(&content).context("Failed to parse pyproject.toml")?;
    if let Some(cfg) = config.tool.irradiate {
        return Ok(cfg);
    }
    if let Some(cfg) = config.tool.mutmut {
        eprintln!(
            "warning: [tool.mutmut] in pyproject.toml is deprecated; rename to [tool.irradiate]"
        );
        return Ok(cfg);
    }
    Ok(IrradiateConfig::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_full_config() {
        let toml_str = r#"
[tool.irradiate]
paths_to_mutate = "lib"
tests_dir = "test"
do_not_mutate = ["vendor/*"]
debug = true
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        let irr = config.tool.irradiate.as_ref().unwrap();
        assert_eq!(irr.paths_to_mutate.as_deref(), Some("lib"));
        assert_eq!(irr.tests_dir.as_deref(), Some("test"));
        assert!(irr.debug.unwrap());
        let do_not_mutate = irr.do_not_mutate.as_ref().unwrap();
        assert_eq!(do_not_mutate.len(), 1);
        assert_eq!(do_not_mutate[0], "vendor/*");
    }

    #[test]
    fn test_missing_irradiate_section() {
        let toml_str = r#"
[tool.ruff]
line-length = 144
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert!(config.tool.irradiate.is_none());
        assert!(config.tool.mutmut.is_none());
    }

    #[test]
    fn test_empty_file() {
        let config: ProjectConfig = toml::from_str("").unwrap();
        assert!(config.tool.irradiate.is_none());
        assert!(config.tool.mutmut.is_none());
    }

    #[test]
    fn test_unknown_keys_ignored() {
        // Unknown keys in [tool.irradiate] must not cause parse failure
        let toml_str = r#"
[tool.irradiate]
paths_to_mutate = "src"
future_unknown_key = "something"
"#;
        let result: Result<ProjectConfig, _> = toml::from_str(toml_str);
        assert!(result.is_ok());
        let config = result.unwrap();
        assert_eq!(
            config
                .tool
                .irradiate
                .as_ref()
                .unwrap()
                .paths_to_mutate
                .as_deref(),
            Some("src")
        );
    }

    #[test]
    fn test_load_config_missing_file() {
        // Missing pyproject.toml is not an error
        let tmp = tempfile::tempdir().unwrap();
        let result = load_config(tmp.path());
        assert!(result.is_ok());
        let cfg = result.unwrap();
        assert!(cfg.paths_to_mutate.is_none());
    }

    #[test]
    fn test_load_config_invalid_toml() {
        // Invalid TOML produces clear error
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("pyproject.toml"), "[[[ invalid").unwrap();
        let result = load_config(tmp.path());
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("pyproject.toml"),
            "error message should mention pyproject.toml: {msg}"
        );
    }

    #[test]
    fn test_all_supported_keys() {
        let toml_str = r#"
[tool.irradiate]
paths_to_mutate = "src"
tests_dir = "tests"
do_not_mutate = ["src/generated/*", "src/vendor/*"]
also_copy = ["data/"]
debug = false
pytest_add_cli_args = "-v --tb=short"
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        let m = config.tool.irradiate.as_ref().unwrap();
        assert_eq!(m.paths_to_mutate.as_deref(), Some("src"));
        assert_eq!(m.tests_dir.as_deref(), Some("tests"));
        assert_eq!(m.do_not_mutate.as_ref().unwrap().len(), 2);
        assert_eq!(m.also_copy.as_ref().unwrap(), &["data/"]);
        assert_eq!(m.debug, Some(false));
        assert_eq!(m.pytest_add_cli_args.as_deref(), Some("-v --tb=short"));
    }

    // INV-4: [tool.mutmut] fallback — load_config must return its values when [tool.irradiate] absent.
    #[test]
    fn test_load_config_mutmut_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("pyproject.toml"),
            "[tool.mutmut]\npaths_to_mutate = \"src\"\n",
        )
        .unwrap();
        let cfg = load_config(tmp.path()).unwrap();
        assert_eq!(cfg.paths_to_mutate.as_deref(), Some("src"));
    }

    // INV-4: [tool.irradiate] takes priority over [tool.mutmut] when both are present.
    #[test]
    fn test_load_config_irradiate_preferred_over_mutmut() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("pyproject.toml"),
            "[tool.irradiate]\npaths_to_mutate = \"irr_src\"\n\n[tool.mutmut]\npaths_to_mutate = \"mutmut_src\"\n",
        )
        .unwrap();
        let cfg = load_config(tmp.path()).unwrap();
        assert_eq!(cfg.paths_to_mutate.as_deref(), Some("irr_src"));
    }
}
