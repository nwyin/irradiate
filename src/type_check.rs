//! Type-check filter: suppress mutants caught by static type checking.
//!
//! Runs a type checker (mypy, pyright, ty) against mutated code and filters
//! out mutants that introduce type errors. These mutants are marked as
//! `MutantStatus::TypeCheck` (exit code 37) and counted as killed.
//!
//! Layer 1 (engines) — depends only on Layer 0 types (protocol, config, cache).

use crate::cache::MutantCacheDescriptor;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A single type checking error reported by the type checker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeCheckError {
    pub file: PathBuf,
    pub line: usize,
    pub message: String,
}

/// Which JSON parser to use for type checker output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParserKind {
    Mypy,
    Pyright,
    Ty,
}

/// Expand a preset name into a full type checker command with JSON output flags.
///
/// Known presets: `mypy`, `pyright`, `ty`.
/// Returns `None` if the name is not a recognized preset.
pub fn expand_preset(name: &str, mutants_dir: &Path) -> Option<Vec<String>> {
    let dir = mutants_dir.display().to_string();
    match name {
        "mypy" => Some(vec![
            "mypy".into(),
            "--output".into(),
            "json".into(),
            dir,
        ]),
        "pyright" => Some(vec!["pyright".into(), "--outputjson".into(), dir]),
        "ty" => Some(vec![
            "ty".into(),
            "check".into(),
            "--output-format".into(),
            "gitlab".into(),
            dir,
        ]),
        _ => None,
    }
}

/// Parse a raw command string into argv, replacing `mutants/` with the actual
/// mutants directory path.
pub fn parse_raw_command(cmd: &str, mutants_dir: &Path) -> Vec<String> {
    let dir = mutants_dir.display().to_string();
    cmd.split_whitespace()
        .map(|tok| {
            if tok == "mutants/" || tok == "mutants" {
                dir.clone()
            } else {
                tok.to_string()
            }
        })
        .collect()
}

/// Resolve a type checker specification (preset name or raw command) into an argv.
pub fn resolve_command(spec: &str, mutants_dir: &Path) -> Vec<String> {
    if let Some(preset) = expand_preset(spec, mutants_dir) {
        preset
    } else {
        parse_raw_command(spec, mutants_dir)
    }
}

/// Detect the parser kind from the command argv.
///
/// Checks whether the command contains "mypy", "pyright", or "ty" as a
/// substring of any argument. Falls back to pyright parser as default.
pub fn detect_parser(command: &[String]) -> ParserKind {
    for arg in command {
        if arg.contains("mypy") {
            return ParserKind::Mypy;
        }
        if arg.contains("pyright") {
            return ParserKind::Pyright;
        }
        // Match "ty" only when it's the whole argument (not substring of e.g. "mypy")
        if arg == "ty" {
            return ParserKind::Ty;
        }
    }
    ParserKind::Pyright
}

/// Run a type checker subprocess and parse its JSON output into errors.
pub fn run_type_checker(command: &[String]) -> Result<Vec<TypeCheckError>> {
    if command.is_empty() {
        bail!("type checker command is empty");
    }

    let output = std::process::Command::new(&command[0])
        .args(&command[1..])
        .output()
        .with_context(|| format!("Failed to run type checker: {}", command[0]))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let parser = detect_parser(command);

    tracing::debug!(
        "Type checker exited with code {:?}, stdout {} bytes, stderr {} bytes",
        output.status.code(),
        stdout.len(),
        stderr.len(),
    );

    match parser {
        ParserKind::Mypy => parse_mypy_output(&stdout),
        ParserKind::Pyright => parse_pyright_output(&stdout),
        ParserKind::Ty => match parse_ty_output(&stdout) {
            Ok(errors) => Ok(errors),
            Err(e) => {
                // ty is experimental — if parsing fails, fall back to empty vec with warning
                tracing::warn!(
                    "ty JSON parsing failed ({}), falling back to empty error list. \
                     ty's output format may have changed.",
                    e
                );
                Ok(vec![])
            }
        },
    }
}

/// Parse mypy newline-delimited JSON output.
///
/// Each line is a JSON object with fields: `file`, `line`, `severity`, `message`.
/// Only `severity == "error"` lines are included.
pub fn parse_mypy_output(stdout: &str) -> Result<Vec<TypeCheckError>> {
    let mut errors = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let val: serde_json::Value = serde_json::from_str(line)
            .with_context(|| format!("Failed to parse mypy JSON line: {line}"))?;
        let severity = val["severity"].as_str().unwrap_or("");
        if severity != "error" {
            continue;
        }
        let file = val["file"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let line_num = val["line"].as_u64().unwrap_or(0) as usize;
        let message = val["message"].as_str().unwrap_or("").to_string();
        errors.push(TypeCheckError {
            file: PathBuf::from(file),
            line: line_num,
            message,
        });
    }
    Ok(errors)
}

/// Parse pyright single JSON object output.
///
/// Expects a `generalDiagnostics` array with entries containing
/// `file`, `range.start.line` (0-indexed), and `message`.
pub fn parse_pyright_output(stdout: &str) -> Result<Vec<TypeCheckError>> {
    let val: serde_json::Value =
        serde_json::from_str(stdout).context("Failed to parse pyright JSON output")?;
    let diagnostics = val["generalDiagnostics"]
        .as_array()
        .context("pyright output missing 'generalDiagnostics' key")?;

    let mut errors = Vec::new();
    for diag in diagnostics {
        let file = diag["file"].as_str().unwrap_or("").to_string();
        // pyright lines are 0-indexed; convert to 1-indexed
        let line = diag["range"]["start"]["line"].as_u64().unwrap_or(0) as usize + 1;
        let message = diag["message"].as_str().unwrap_or("").to_string();
        errors.push(TypeCheckError {
            file: PathBuf::from(file),
            line,
            message,
        });
    }
    Ok(errors)
}

/// Parse ty gitlab code quality JSON output.
///
/// Expects a JSON array of objects with `severity`, `location.path`,
/// `location.positions.begin.line`, and `description`.
/// Only severities "major", "critical", "blocker" are included.
pub fn parse_ty_output(stdout: &str) -> Result<Vec<TypeCheckError>> {
    let val: serde_json::Value =
        serde_json::from_str(stdout).context("Failed to parse ty JSON output")?;
    let arr = val
        .as_array()
        .context("ty output is not a JSON array")?;

    let mut errors = Vec::new();
    for item in arr {
        let severity = item["severity"].as_str().unwrap_or("");
        if !matches!(severity, "major" | "critical" | "blocker") {
            continue;
        }
        let file = item["location"]["path"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let line = item["location"]["positions"]["begin"]["line"]
            .as_u64()
            .unwrap_or(0) as usize;
        let message = item["description"].as_str().unwrap_or("").to_string();
        errors.push(TypeCheckError {
            file: PathBuf::from(file),
            line,
            message,
        });
    }
    Ok(errors)
}

// --- Baseline caching ---

/// Compute a SHA256 hash of the contents of all source files.
/// Files are sorted by path for determinism.
pub fn compute_source_hash(paths: &[PathBuf]) -> Result<String> {
    let mut all_files: Vec<PathBuf> = Vec::new();
    for p in paths {
        if p.is_file() {
            all_files.push(p.to_path_buf());
        } else if p.is_dir() {
            collect_python_files(p, &mut all_files)?;
        }
    }
    all_files.sort();

    let mut hasher = Sha256::new();
    for f in &all_files {
        let content = std::fs::read(f)
            .with_context(|| format!("Failed to read source file: {}", f.display()))?;
        // Include path in hash so renames are detected
        let rel = f.display().to_string();
        hasher.update((rel.len() as u64).to_le_bytes());
        hasher.update(rel.as_bytes());
        hasher.update((content.len() as u64).to_le_bytes());
        hasher.update(&content);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn collect_python_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("Failed to read directory: {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_python_files(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "py") {
            out.push(path);
        }
    }
    Ok(())
}

/// Baseline cache file path within the irradiate cache directory.
fn baseline_cache_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("type_check_baseline.json")
}

/// Cached baseline data.
#[derive(Debug, Serialize, Deserialize)]
struct BaselineCache {
    source_hash: String,
    errors: Vec<TypeCheckError>,
}

/// Load a cached baseline if the source hash matches.
pub fn load_cached_baseline(
    cache_dir: &Path,
    source_hash: &str,
) -> Option<Vec<TypeCheckError>> {
    let path = baseline_cache_path(cache_dir);
    let content = std::fs::read_to_string(&path).ok()?;
    let cached: BaselineCache = serde_json::from_str(&content).ok()?;
    if cached.source_hash == source_hash {
        Some(cached.errors)
    } else {
        None
    }
}

/// Save baseline errors to cache.
pub fn save_baseline_cache(
    cache_dir: &Path,
    source_hash: &str,
    errors: &[TypeCheckError],
) -> Result<()> {
    std::fs::create_dir_all(cache_dir)
        .with_context(|| format!("Failed to create cache dir: {}", cache_dir.display()))?;
    let cached = BaselineCache {
        source_hash: source_hash.to_string(),
        errors: errors.to_vec(),
    };
    let data = serde_json::to_string_pretty(&cached)?;
    std::fs::write(baseline_cache_path(cache_dir), data)?;
    Ok(())
}

// --- Error diffing ---

/// Subtract baseline errors from mutant errors.
///
/// Matches by (file basename, line, message) so that pre-existing type errors
/// in the user's codebase don't cause false suppression.
pub fn diff_errors(
    mutant_errors: &[TypeCheckError],
    baseline_errors: &[TypeCheckError],
) -> Vec<TypeCheckError> {
    let baseline_set: HashSet<(String, usize, &str)> = baseline_errors
        .iter()
        .map(|e| {
            let basename = e
                .file
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            (basename, e.line, e.message.as_str())
        })
        .collect();

    mutant_errors
        .iter()
        .filter(|e| {
            let basename = e
                .file
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            !baseline_set.contains(&(basename, e.line, e.message.as_str()))
        })
        .cloned()
        .collect()
}

// --- Map errors to mutants ---

/// Map type check errors to mutant names.
///
/// For each error, finds mutants whose function span includes the error line.
/// The error file path is mapped back from `mutants/` to the source file by
/// stripping the mutants_dir prefix and comparing to `MutantCacheDescriptor.source_file`.
///
/// Returns deduplicated mutant names.
pub fn map_errors_to_mutants(
    errors: &[TypeCheckError],
    descriptors: &[MutantCacheDescriptor],
    mutants_dir: &Path,
) -> Vec<String> {
    let mut matched: HashSet<String> = HashSet::new();

    for error in errors {
        // Strip mutants_dir prefix to get relative path (e.g. "simple_lib.py")
        let error_rel = error
            .file
            .strip_prefix(mutants_dir)
            .unwrap_or(&error.file);

        for desc in descriptors {
            // Compare: the error's relative path should match the source_file
            // after normalizing — source_file is like "src/simple_lib.py" but
            // the mutants dir mirrors the structure, so the error relative path
            // within mutants/ should match the tail of source_file.
            if !paths_match(error_rel, &desc.source_file) {
                continue;
            }

            // Check if the error line falls within the function's line range.
            let fn_start = desc.fn_start_line;
            if fn_start == 0 {
                continue;
            }
            let fn_line_count = desc.function_source.lines().count();
            let fn_end = fn_start + fn_line_count.saturating_sub(1);

            if error.line >= fn_start && error.line <= fn_end {
                matched.insert(desc.mutant_name.clone());
            }
        }
    }

    let mut result: Vec<String> = matched.into_iter().collect();
    result.sort();
    result
}

/// Check whether an error's relative path (from mutants dir) matches a source file path.
///
/// The mutants directory mirrors the source layout, so `simple_lib.py` in mutants/
/// corresponds to `src/simple_lib.py` (or just `simple_lib.py`) in the source.
/// We compare by file name and any shared suffix components.
fn paths_match(error_rel: &Path, source_file: &str) -> bool {
    let source_path = Path::new(source_file);
    // Simple case: file names match
    let error_name = error_rel.file_name().unwrap_or_default();
    let source_name = source_path.file_name().unwrap_or_default();
    if error_name != source_name {
        return false;
    }
    // Check that the error path is a suffix of (or equal to) the source path
    // or vice versa. This handles both "simple_lib.py" matching "src/simple_lib.py"
    // and "pkg/mod.py" matching "src/pkg/mod.py".
    let error_str = error_rel.to_string_lossy().replace('\\', "/");
    let source_str = source_file.replace('\\', "/");
    source_str.ends_with(&error_str) || error_str.ends_with(&source_str)
}

/// Extract a human-readable tool name from the type checker spec.
pub fn tool_name_from_spec(spec: &str) -> &str {
    match spec {
        "mypy" | "pyright" | "ty" => spec,
        _ => {
            // Try to extract the tool name from the first word
            spec.split_whitespace().next().unwrap_or(spec)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Preset expansion ---

    #[test]
    fn test_expand_preset_mypy() {
        let dir = PathBuf::from("/tmp/mutants");
        let cmd = expand_preset("mypy", &dir).unwrap();
        assert_eq!(cmd, vec!["mypy", "--output", "json", "/tmp/mutants"]);
    }

    #[test]
    fn test_expand_preset_pyright() {
        let dir = PathBuf::from("/tmp/mutants");
        let cmd = expand_preset("pyright", &dir).unwrap();
        assert_eq!(cmd, vec!["pyright", "--outputjson", "/tmp/mutants"]);
    }

    #[test]
    fn test_expand_preset_ty() {
        let dir = PathBuf::from("/tmp/mutants");
        let cmd = expand_preset("ty", &dir).unwrap();
        assert_eq!(
            cmd,
            vec!["ty", "check", "--output-format", "gitlab", "/tmp/mutants"]
        );
    }

    #[test]
    fn test_expand_preset_unknown_returns_none() {
        assert!(expand_preset("ruff", &PathBuf::from("/tmp/mutants")).is_none());
    }

    // --- Raw command parsing ---

    #[test]
    fn test_parse_raw_command_replaces_mutants_dir() {
        let cmd = parse_raw_command("mypy --strict --output json mutants/", &PathBuf::from("/proj/mutants"));
        assert_eq!(
            cmd,
            vec!["mypy", "--strict", "--output", "json", "/proj/mutants"]
        );
    }

    #[test]
    fn test_parse_raw_command_no_replacement() {
        let cmd = parse_raw_command("pyright --outputjson /custom/path", &PathBuf::from("/tmp"));
        assert_eq!(cmd, vec!["pyright", "--outputjson", "/custom/path"]);
    }

    // --- Parser detection ---

    #[test]
    fn test_detect_parser_mypy() {
        let cmd = vec!["mypy".into(), "--output".into(), "json".into()];
        assert_eq!(detect_parser(&cmd), ParserKind::Mypy);
    }

    #[test]
    fn test_detect_parser_pyright() {
        let cmd = vec!["pyright".into(), "--outputjson".into()];
        assert_eq!(detect_parser(&cmd), ParserKind::Pyright);
    }

    #[test]
    fn test_detect_parser_ty() {
        let cmd = vec!["ty".into(), "check".into()];
        assert_eq!(detect_parser(&cmd), ParserKind::Ty);
    }

    #[test]
    fn test_detect_parser_default_is_pyright() {
        let cmd = vec!["custom-checker".into()];
        assert_eq!(detect_parser(&cmd), ParserKind::Pyright);
    }

    // --- Mypy parser ---

    #[test]
    fn test_parse_mypy_valid() {
        let input = r#"{"file": "src/foo.py", "line": 10, "column": 5, "message": "Incompatible return type", "severity": "error", "code": "return-value"}
{"file": "src/foo.py", "line": 12, "column": 1, "message": "some note", "severity": "note", "code": "misc"}"#;
        let errors = parse_mypy_output(input).unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].file, PathBuf::from("src/foo.py"));
        assert_eq!(errors[0].line, 10);
        assert_eq!(errors[0].message, "Incompatible return type");
    }

    #[test]
    fn test_parse_mypy_empty() {
        let errors = parse_mypy_output("").unwrap();
        assert!(errors.is_empty());
    }

    #[test]
    fn test_parse_mypy_non_error_filtered() {
        let input = r#"{"file": "a.py", "line": 1, "message": "note", "severity": "note"}"#;
        let errors = parse_mypy_output(input).unwrap();
        assert!(errors.is_empty());
    }

    // --- Pyright parser ---

    #[test]
    fn test_parse_pyright_valid() {
        let input = r#"{
            "version": "1.1.0",
            "generalDiagnostics": [
                {
                    "file": "src/bar.py",
                    "range": {"start": {"line": 4, "character": 0}, "end": {"line": 4, "character": 10}},
                    "message": "Type mismatch",
                    "severity": 1
                }
            ]
        }"#;
        let errors = parse_pyright_output(input).unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].file, PathBuf::from("src/bar.py"));
        assert_eq!(errors[0].line, 5); // 0-indexed + 1
        assert_eq!(errors[0].message, "Type mismatch");
    }

    #[test]
    fn test_parse_pyright_missing_key() {
        let input = r#"{"version": "1.0"}"#;
        let result = parse_pyright_output(input);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("generalDiagnostics"),
            "error should mention the missing key"
        );
    }

    // --- Ty parser ---

    #[test]
    fn test_parse_ty_valid() {
        let input = r#"[
            {
                "severity": "major",
                "location": {
                    "path": "src/baz.py",
                    "positions": {"begin": {"line": 7, "column": 1}}
                },
                "description": "Type error in expression"
            },
            {
                "severity": "info",
                "location": {
                    "path": "src/baz.py",
                    "positions": {"begin": {"line": 8, "column": 1}}
                },
                "description": "Informational note"
            }
        ]"#;
        let errors = parse_ty_output(input).unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].file, PathBuf::from("src/baz.py"));
        assert_eq!(errors[0].line, 7);
        assert_eq!(errors[0].message, "Type error in expression");
    }

    #[test]
    fn test_parse_ty_parse_failure_is_error() {
        // parse_ty_output itself returns Err; the fallback is handled in run_type_checker
        let result = parse_ty_output("not json");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_ty_all_severities() {
        let input = r#"[
            {"severity": "major", "location": {"path": "a.py", "positions": {"begin": {"line": 1, "column": 1}}}, "description": "a"},
            {"severity": "critical", "location": {"path": "a.py", "positions": {"begin": {"line": 2, "column": 1}}}, "description": "b"},
            {"severity": "blocker", "location": {"path": "a.py", "positions": {"begin": {"line": 3, "column": 1}}}, "description": "c"},
            {"severity": "minor", "location": {"path": "a.py", "positions": {"begin": {"line": 4, "column": 1}}}, "description": "d"},
            {"severity": "info", "location": {"path": "a.py", "positions": {"begin": {"line": 5, "column": 1}}}, "description": "e"}
        ]"#;
        let errors = parse_ty_output(input).unwrap();
        assert_eq!(errors.len(), 3);
    }

    // --- Baseline diffing ---

    #[test]
    fn test_diff_errors_subtracts_baseline() {
        let baseline = vec![
            TypeCheckError {
                file: PathBuf::from("src/foo.py"),
                line: 10,
                message: "Pre-existing error".into(),
            },
        ];
        let mutant_errors = vec![
            TypeCheckError {
                file: PathBuf::from("mutants/foo.py"),
                line: 10,
                message: "Pre-existing error".into(),
            },
            TypeCheckError {
                file: PathBuf::from("mutants/foo.py"),
                line: 15,
                message: "New mutation error".into(),
            },
        ];
        let diff = diff_errors(&mutant_errors, &baseline);
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].message, "New mutation error");
    }

    #[test]
    fn test_diff_errors_empty_baseline() {
        let errors = vec![TypeCheckError {
            file: PathBuf::from("a.py"),
            line: 1,
            message: "err".into(),
        }];
        let diff = diff_errors(&errors, &[]);
        assert_eq!(diff.len(), 1);
    }

    #[test]
    fn test_diff_errors_all_baseline() {
        let errors = vec![TypeCheckError {
            file: PathBuf::from("a.py"),
            line: 1,
            message: "err".into(),
        }];
        let diff = diff_errors(&errors, &errors);
        assert!(diff.is_empty());
    }

    // --- Mutant mapping ---

    #[test]
    fn test_map_errors_to_mutants_by_line_range() {
        let mutants_dir = PathBuf::from("/proj/mutants");
        let descriptors = vec![
            MutantCacheDescriptor {
                mutant_name: "simple_lib.x_add__irradiate_1".into(),
                source_file: "src/simple_lib.py".into(),
                fn_start_line: 5,
                fn_byte_offset: 0,
                function_source: "def add(a, b):\n    return a + b\n".into(),
                operator: "binop_swap".into(),
                start: 20,
                end: 21,
                original: "+".into(),
                replacement: "-".into(),
            },
            MutantCacheDescriptor {
                mutant_name: "simple_lib.x_sub__irradiate_1".into(),
                source_file: "src/simple_lib.py".into(),
                fn_start_line: 10,
                fn_byte_offset: 0,
                function_source: "def sub(a, b):\n    return a - b\n".into(),
                operator: "binop_swap".into(),
                start: 20,
                end: 21,
                original: "-".into(),
                replacement: "+".into(),
            },
        ];

        let errors = vec![TypeCheckError {
            file: PathBuf::from("/proj/mutants/simple_lib.py"),
            line: 6,
            message: "Incompatible return type".into(),
        }];

        let matched = map_errors_to_mutants(&errors, &descriptors, &mutants_dir);
        assert_eq!(matched, vec!["simple_lib.x_add__irradiate_1"]);
    }

    #[test]
    fn test_map_errors_no_match() {
        let mutants_dir = PathBuf::from("/proj/mutants");
        let descriptors = vec![MutantCacheDescriptor {
            mutant_name: "mod.x_f__irradiate_1".into(),
            source_file: "src/mod.py".into(),
            fn_start_line: 10,
            fn_byte_offset: 0,
            function_source: "def f():\n    pass\n".into(),
            operator: "noop".into(),
            start: 0,
            end: 0,
            original: "".into(),
            replacement: "".into(),
        }];

        // Error on line 50 — outside the function range (10-11)
        let errors = vec![TypeCheckError {
            file: PathBuf::from("/proj/mutants/mod.py"),
            line: 50,
            message: "err".into(),
        }];

        let matched = map_errors_to_mutants(&errors, &descriptors, &mutants_dir);
        assert!(matched.is_empty());
    }

    // --- Paths match ---

    #[test]
    fn test_paths_match_simple() {
        assert!(paths_match(
            Path::new("simple_lib.py"),
            "src/simple_lib.py"
        ));
    }

    #[test]
    fn test_paths_match_nested() {
        assert!(paths_match(
            Path::new("pkg/mod.py"),
            "src/pkg/mod.py"
        ));
    }

    #[test]
    fn test_paths_no_match() {
        assert!(!paths_match(Path::new("other.py"), "src/simple_lib.py"));
    }

    // --- Baseline cache round-trip ---

    #[test]
    fn test_baseline_cache_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let errors = vec![TypeCheckError {
            file: PathBuf::from("a.py"),
            line: 1,
            message: "err".into(),
        }];
        save_baseline_cache(tmp.path(), "abc123", &errors).unwrap();
        let loaded = load_cached_baseline(tmp.path(), "abc123").unwrap();
        assert_eq!(loaded, errors);
    }

    #[test]
    fn test_baseline_cache_hash_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let errors = vec![TypeCheckError {
            file: PathBuf::from("a.py"),
            line: 1,
            message: "err".into(),
        }];
        save_baseline_cache(tmp.path(), "abc123", &errors).unwrap();
        let loaded = load_cached_baseline(tmp.path(), "different_hash");
        assert!(loaded.is_none());
    }

    #[test]
    fn test_baseline_cache_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let loaded = load_cached_baseline(tmp.path(), "abc123");
        assert!(loaded.is_none());
    }

    // --- Source hash ---

    #[test]
    fn test_compute_source_hash_deterministic() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.py");
        std::fs::write(&f, "x = 1\n").unwrap();
        let h1 = compute_source_hash(&[f.clone()]).unwrap();
        let h2 = compute_source_hash(&[f]).unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_compute_source_hash_changes_with_content() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.py");
        std::fs::write(&f, "x = 1\n").unwrap();
        let h1 = compute_source_hash(&[f.clone()]).unwrap();
        std::fs::write(&f, "x = 2\n").unwrap();
        let h2 = compute_source_hash(&[f]).unwrap();
        assert_ne!(h1, h2);
    }

    // --- Tool name ---

    #[test]
    fn test_tool_name_preset() {
        assert_eq!(tool_name_from_spec("mypy"), "mypy");
        assert_eq!(tool_name_from_spec("pyright"), "pyright");
        assert_eq!(tool_name_from_spec("ty"), "ty");
    }

    #[test]
    fn test_tool_name_raw_command() {
        assert_eq!(
            tool_name_from_spec("mypy --strict --output json mutants/"),
            "mypy"
        );
    }
}
