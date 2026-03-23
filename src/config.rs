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
    /// Accepts a string (`"src"`) or array of strings (`["src/a.py", "src/b.py"]`).
    #[serde(default, deserialize_with = "deserialize_string_or_vec_opt_str")]
    pub paths_to_mutate: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_string_or_first_of_vec")]
    pub tests_dir: Option<String>,
    pub do_not_mutate: Option<Vec<String>>,
    pub also_copy: Option<Vec<String>>,
    pub debug: Option<bool>,
    /// Extra arguments appended to every pytest invocation.
    /// Prefer a TOML array: `pytest_add_cli_args = ["-v", "--tb=short"]`.
    /// A plain string is accepted for backward compatibility and split on whitespace.
    #[serde(default, deserialize_with = "deserialize_string_or_vec_opt")]
    pub pytest_add_cli_args: Option<Vec<String>>,
}

/// Deserialize a field that accepts either a TOML string or array of strings.
/// When given an array, takes the first element only.
fn deserialize_string_or_first_of_vec<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};

    struct StringOrFirstOfVec;

    impl<'de> Visitor<'de> for StringOrFirstOfVec {
        type Value = Option<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(f, "a string or array of strings")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            if let Some(first) = seq.next_element::<String>()? {
                while seq.next_element::<String>()?.is_some() {}
                Ok(Some(first))
            } else {
                Ok(None)
            }
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
    }

    deserializer.deserialize_any(StringOrFirstOfVec)
}

/// Deserialize `paths_to_mutate` from either a TOML string (wrapped in a Vec)
/// or an array of strings.
fn deserialize_string_or_vec_opt_str<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};

    struct StringOrVecStr;

    impl<'de> Visitor<'de> for StringOrVecStr {
        type Value = Option<Vec<String>>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(f, "a string or array of strings")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(Some(vec![v.to_string()]))
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut v = Vec::new();
            while let Some(item) = seq.next_element::<String>()? {
                v.push(item);
            }
            if v.is_empty() {
                Ok(None)
            } else {
                Ok(Some(v))
            }
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
    }

    deserializer.deserialize_any(StringOrVecStr)
}

/// Deserialize `pytest_add_cli_args` from either a TOML string (split on whitespace,
/// deprecated) or a TOML array of strings (preferred).
fn deserialize_string_or_vec_opt<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};

    struct StringOrVecOpt;

    impl<'de> Visitor<'de> for StringOrVecOpt {
        type Value = Option<Vec<String>>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(f, "a string or array of strings")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            eprintln!(
                "warning: pytest_add_cli_args as a string is deprecated; \
                 use a TOML array instead: pytest_add_cli_args = [\"-v\", \"--tb=short\"]"
            );
            Ok(Some(v.split_whitespace().map(str::to_string).collect()))
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut v = Vec::new();
            while let Some(item) = seq.next_element::<String>()? {
                v.push(item);
            }
            Ok(Some(v))
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
    }

    deserializer.deserialize_any(StringOrVecOpt)
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
        assert_eq!(irr.paths_to_mutate, Some(vec!["lib".to_string()]));
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
                .paths_to_mutate,
            Some(vec!["src".to_string()])
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
pytest_add_cli_args = ["-v", "--tb=short"]
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        let m = config.tool.irradiate.as_ref().unwrap();
        assert_eq!(m.paths_to_mutate, Some(vec!["src".to_string()]));
        assert_eq!(m.tests_dir.as_deref(), Some("tests"));
        assert_eq!(m.do_not_mutate.as_ref().unwrap().len(), 2);
        assert_eq!(m.also_copy.as_ref().unwrap(), &["data/"]);
        assert_eq!(m.debug, Some(false));
        let args = m.pytest_add_cli_args.as_ref().unwrap();
        assert_eq!(args, &["-v", "--tb=short"]);
    }

    #[test]
    fn test_pytest_add_cli_args_string_compat() {
        // String form is still accepted for backward compatibility (split on whitespace)
        let toml_str = r#"
[tool.irradiate]
pytest_add_cli_args = "-v --tb=short"
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        let args = config.tool.irradiate.as_ref().unwrap().pytest_add_cli_args.as_ref().unwrap();
        assert_eq!(args, &["-v", "--tb=short"]);
    }

    #[test]
    fn test_pytest_add_cli_args_absent() {
        // Missing field deserializes to None (default)
        let toml_str = r#"
[tool.irradiate]
paths_to_mutate = "src"
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        assert!(config.tool.irradiate.as_ref().unwrap().pytest_add_cli_args.is_none());
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
        assert_eq!(cfg.paths_to_mutate, Some(vec!["src".to_string()]));
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
        assert_eq!(cfg.paths_to_mutate, Some(vec!["irr_src".to_string()]));
    }

    #[test]
    fn test_paths_to_mutate_array() {
        let toml_str = r#"
[tool.irradiate]
paths_to_mutate = ["src/a.py", "src/b.py"]
"#;
        let config: ProjectConfig = toml::from_str(toml_str).unwrap();
        let irr = config.tool.irradiate.as_ref().unwrap();
        assert_eq!(
            irr.paths_to_mutate,
            Some(vec!["src/a.py".to_string(), "src/b.py".to_string()])
        );
    }
}
