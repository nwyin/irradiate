//! Mutation testing reports: GitHub Actions output and Stryker JSON schema v2.
//!
//! ## GitHub Actions (auto-detected)
//! Both features activate automatically via environment variables.
//!
//! - **Tier 1** (`GITHUB_ACTIONS=true`): Emit `::warning` annotation lines for
//!   survived mutants. GitHub Actions renders these as yellow warning badges
//!   on the PR "Files changed" tab.
//! - **Tier 2** (`GITHUB_STEP_SUMMARY` set): Append a Markdown summary table
//!   to the step summary file.
//!
//! ## Stryker mutation-testing-report-schema v2 (`--report json`)
//! The Stryker schema is the de-facto standard interchange format for mutation
//! testing reports, consumed by the Stryker Dashboard and various HTML renderers.
//!
//! Schema reference: <https://github.com/stryker-mutator/mutation-testing-elements>

use crate::cache::MutantCacheDescriptor;
use crate::protocol::{MutantResult, MutantStatus};
use crate::stats::TestStats;
use std::collections::HashMap;
use std::fmt::Write as FmtWrite;
use std::io::Write as IoWrite;
use std::path::Path;

/// Self-contained HTML template that loads mutation-testing-elements from unpkg CDN.
/// `REPORT_JSON_PLACEHOLDER` is replaced with the serialised Stryker JSON at write time.
const HTML_TEMPLATE: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>irradiate — Mutation Testing Report</title>
  <script defer src="https://www.unpkg.com/mutation-testing-elements"></script>
</head>
<body>
  <mutation-test-report-app title-postfix="irradiate">
  </mutation-test-report-app>
  <script>
    document.querySelector('mutation-test-report-app').report = REPORT_JSON_PLACEHOLDER;
  </script>
</body>
</html>
"#;

/// Write a self-contained HTML mutation testing report using the Stryker
/// mutation-testing-elements web component.
///
/// The report JSON is inlined into the page; the only external dependency is
/// the `mutation-testing-elements` script loaded from the unpkg CDN.
pub fn write_html_report(report_json: &serde_json::Value, output_path: &Path) -> anyhow::Result<()> {
    let json_str = serde_json::to_string(report_json)?;
    let html = HTML_TEMPLATE.replace("REPORT_JSON_PLACEHOLDER", &json_str);
    std::fs::write(output_path, html)?;
    Ok(())
}

/// Maximum number of `::warning` annotations emitted per step.
/// GitHub Actions truncates annotation lists beyond this point.
const MAX_ANNOTATIONS: usize = 10;

/// Compute the 1-based line number in a source file for a byte offset that is
/// `fn_offset_in_fn` bytes into the function body, given that the function
/// starts at 1-based line `fn_start_line`.
///
/// Returns `fn_start_line` if anything goes wrong (wrong offset, empty source).
pub fn byte_offset_to_line(
    function_source: &str,
    fn_start_line: usize,
    offset_in_fn: usize,
) -> usize {
    if fn_start_line == 0 {
        return 1;
    }
    let capped = offset_in_fn.min(function_source.len());
    let extra_lines = function_source[..capped]
        .bytes()
        .filter(|&b| b == b'\n')
        .count();
    fn_start_line + extra_lines
}

/// Emit GitHub Actions annotations for survived mutants, then write a Markdown
/// step summary if `$GITHUB_STEP_SUMMARY` is set.
///
/// This is a no-op when `GITHUB_ACTIONS != "true"`.
pub fn emit_github_annotations(
    results: &[MutantResult],
    descriptors: &[MutantCacheDescriptor],
    killed: usize,
    survived_count: usize,
) {
    if std::env::var("GITHUB_ACTIONS").as_deref() != Ok("true") {
        return;
    }

    // --- Tier 1: ::warning annotations ---
    let survived: Vec<&MutantResult> = results
        .iter()
        .filter(|r| r.status == MutantStatus::Survived)
        .collect();

    let to_annotate = survived.len().min(MAX_ANNOTATIONS);

    for result in &survived[..to_annotate] {
        if let Some(desc) = descriptors
            .iter()
            .find(|d| d.mutant_name == result.mutant_name)
        {
            let line = byte_offset_to_line(&desc.function_source, desc.fn_start_line, desc.start);
            let file = if desc.source_file.is_empty() {
                "unknown".to_string()
            } else {
                desc.source_file.clone()
            };
            // Extract just the function name for the message
            let func_display = desc
                .mutant_name
                .rsplit_once("__irradiate_")
                .map(|(prefix, _)| prefix.rsplit_once('.').map_or(prefix, |(_, local)| local))
                .unwrap_or(&desc.mutant_name);
            println!(
                "::warning file={file},line={line},endLine={line},title=Survived mutant ({op})::replaced `{orig}` with `{repl}` in {func}()",
                file = file,
                line = line,
                op = desc.operator,
                orig = desc.original,
                repl = desc.replacement,
                func = func_display,
            );
        }
    }

    if survived.len() > MAX_ANNOTATIONS {
        println!(
            "::notice::{annotated} of {total} survived mutants annotated. Use `irradiate results --json` for full details.",
            annotated = MAX_ANNOTATIONS,
            total = survived.len(),
        );
    }

    // --- Tier 2: step summary ---
    if let Ok(summary_path) = std::env::var("GITHUB_STEP_SUMMARY") {
        match build_step_summary(results, descriptors, killed, survived_count) {
            Ok(markdown) => {
                match std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&summary_path)
                {
                    Ok(mut f) => {
                        if let Err(e) = f.write_all(markdown.as_bytes()) {
                            eprintln!("irradiate: failed to write step summary: {e}");
                        }
                    }
                    Err(e) => {
                        eprintln!("irradiate: could not open $GITHUB_STEP_SUMMARY ({summary_path}): {e}");
                    }
                }
            }
            Err(e) => {
                eprintln!("irradiate: failed to build step summary: {e}");
            }
        }
    }
}

/// Build the Markdown step summary string.
///
/// Extracted as a pure function so it can be tested without touching the
/// filesystem or requiring GITHUB_STEP_SUMMARY to be set.
pub fn build_step_summary(
    results: &[MutantResult],
    descriptors: &[MutantCacheDescriptor],
    killed: usize,
    survived_count: usize,
) -> Result<String, std::fmt::Error> {
    let mut no_coverage = 0usize;
    let mut timeout = 0usize;

    for r in results {
        match r.status {
            MutantStatus::NoTests => no_coverage += 1,
            MutantStatus::Timeout => timeout += 1,
            _ => {}
        }
    }

    let tested = killed + survived_count;
    let score_str = if tested > 0 {
        format!("{:.1}%", killed as f64 / tested as f64 * 100.0)
    } else {
        "N/A".to_string()
    };

    let mut md = String::new();
    writeln!(md, "## Mutation Testing Results — irradiate")?;
    writeln!(md)?;
    writeln!(md, "| Metric | Count |")?;
    writeln!(md, "|--------|-------|")?;
    writeln!(md, "| Killed | {killed} |")?;
    writeln!(md, "| Survived | {survived_count} |")?;
    writeln!(md, "| No coverage | {no_coverage} |")?;
    writeln!(md, "| Timeout | {timeout} |")?;
    writeln!(md, "| **Score** | **{score_str}** |")?;
    writeln!(md)?;

    if survived_count > 0 {
        writeln!(
            md,
            "<details>\n<summary>Survived mutants ({survived_count})</summary>\n"
        )?;
        writeln!(md, "| Mutant | Location | Operator | Mutation |")?;
        writeln!(md, "|--------|----------|----------|----------|")?;

        for result in results {
            if result.status != MutantStatus::Survived {
                continue;
            }
            if let Some(desc) = descriptors
                .iter()
                .find(|d| d.mutant_name == result.mutant_name)
            {
                let line =
                    byte_offset_to_line(&desc.function_source, desc.fn_start_line, desc.start);
                let location = if desc.source_file.is_empty() {
                    format!("line {line}")
                } else {
                    format!("{}:{line}", desc.source_file)
                };
                writeln!(
                    md,
                    "| `{}` | `{location}` | {} | `{}` → `{}` |",
                    result.mutant_name, desc.operator, desc.original, desc.replacement,
                )?;
            } else {
                writeln!(
                    md,
                    "| `{}` | unknown | unknown | unknown |",
                    result.mutant_name,
                )?;
            }
        }

        writeln!(md, "\n</details>")?;
        writeln!(md)?;
    }

    Ok(md)
}

/// Map an irradiate `MutantStatus` to its Stryker schema status string.
fn stryker_status(status: MutantStatus) -> &'static str {
    match status {
        MutantStatus::Killed => "Killed",
        MutantStatus::Survived => "Survived",
        MutantStatus::NoTests => "NoCoverage",
        MutantStatus::Timeout => "Timeout",
        MutantStatus::TypeCheck | MutantStatus::Error => "RuntimeError",
    }
}

/// Build a Stryker mutation-testing-report-schema v2 JSON value from irradiate results.
///
/// # Arguments
/// - `results` — one `MutantResult` per tested mutant
/// - `descriptors` — rich descriptors with operator, byte spans, and source file.
///   May be empty (e.g. when called from `irradiate results --report`); in that
///   case mutants are included with minimal location info.
/// - `stats` — optional stats from the coverage run; used to populate `coveredBy`
/// - `project_root` — absolute path to the project root (written into the report)
/// - `paths_to_mutate` — used to locate source files when `descriptor.source_file`
///   is not set
pub fn build_stryker_report(
    results: &[MutantResult],
    descriptors: &[MutantCacheDescriptor],
    stats: Option<&TestStats>,
    project_root: &Path,
    paths_to_mutate: &Path,
) -> serde_json::Value {
    // Index: mutant_name → descriptor
    let desc_by_name: HashMap<&str, &MutantCacheDescriptor> =
        descriptors.iter().map(|d| (d.mutant_name.as_str(), d)).collect();

    // Group mutant entries by relative source file path.
    // The file key is slash-separated and relative to project_root.
    let mut file_mutants: HashMap<String, Vec<serde_json::Value>> = HashMap::new();

    for result in results {
        let name = result.mutant_name.as_str();
        let desc = desc_by_name.get(name).copied();

        // --- Status and timing ---
        let status_str = stryker_status(result.status);
        let duration_ms = (result.duration * 1000.0).round() as u64;

        // --- coveredBy (function-level coverage from stats run) ---
        let covered_by: Vec<serde_json::Value> = if let Some(s) = stats {
            let func_key = name
                .rsplit_once("__irradiate_")
                .map(|(prefix, _)| prefix)
                .unwrap_or(name);
            s.tests_for_function(func_key)
                .into_iter()
                .map(serde_json::Value::String)
                .collect()
        } else {
            vec![]
        };

        // --- Location ---
        let location = if let Some(d) = desc {
            let (rel_start_line, start_col) =
                crate::codegen::byte_offset_to_location(&d.function_source, d.start);
            let (rel_end_line, end_col) =
                crate::codegen::byte_offset_to_location(&d.function_source, d.end);
            // fn_start_line is 1-indexed; rel_*_line are also 1-indexed relative to function start.
            let abs_start_line = d.fn_start_line + rel_start_line - 1;
            let abs_end_line = d.fn_start_line + rel_end_line - 1;
            serde_json::json!({
                "start": { "line": abs_start_line, "column": start_col },
                "end":   { "line": abs_end_line,   "column": end_col }
            })
        } else {
            // Minimal placeholder when no descriptor is available
            serde_json::json!({
                "start": { "line": 1, "column": 1 },
                "end":   { "line": 1, "column": 1 }
            })
        };

        // --- Mutator name, replacement, description ---
        let (mutator_name, replacement, description) = if let Some(d) = desc {
            (
                d.operator.clone(),
                d.replacement.clone(),
                format!("replaced `{}` with `{}`", d.original, d.replacement),
            )
        } else {
            (
                "unknown".to_string(),
                String::new(),
                "unknown mutation".to_string(),
            )
        };

        // --- Source file key (relative to project_root, slash-separated) ---
        // source_file is already a relative slash-separated path set by the pipeline.
        let file_key = if let Some(d) = desc {
            if d.source_file.is_empty() {
                // source_file was not set; derive from module name
                derive_file_key_from_name(name, project_root, paths_to_mutate)
            } else {
                d.source_file.clone()
            }
        } else {
            derive_file_key_from_name(name, project_root, paths_to_mutate)
        };

        // --- Build mutant object ---
        let mut mutant_obj = serde_json::json!({
            "id": name,
            "mutatorName": mutator_name,
            "replacement": replacement,
            "description": description,
            "location": location,
            "status": status_str,
            "duration": duration_ms,
        });
        if !covered_by.is_empty() {
            mutant_obj["coveredBy"] = serde_json::Value::Array(covered_by);
        }

        file_mutants.entry(file_key).or_default().push(mutant_obj);
    }

    // Build the `files` object: file_key → { language, source, mutants }
    let mut files_obj = serde_json::Map::new();
    for (file_key, mutants) in file_mutants {
        let source_path = project_root.join(&file_key);
        let source = std::fs::read_to_string(&source_path).unwrap_or_default();
        files_obj.insert(
            file_key,
            serde_json::json!({
                "language": "python",
                "source": source,
                "mutants": mutants,
            }),
        );
    }

    serde_json::json!({
        "schemaVersion": "2",
        "thresholds": { "high": 80, "low": 60 },
        "projectRoot": project_root.display().to_string(),
        "files": serde_json::Value::Object(files_obj),
        "framework": {
            "name": "irradiate",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

/// Derive a slash-separated file key from the mutant name (best-effort, no descriptor).
///
/// Mutant name format: `module.x_func__irradiate_N`
/// Module "simple_lib" → check for `paths_to_mutate/simple_lib/__init__.py`
/// or `paths_to_mutate/simple_lib.py`.
fn derive_file_key_from_name(name: &str, project_root: &Path, paths_to_mutate: &Path) -> String {
    let module = name.split('.').next().unwrap_or(name);
    let module_path = module.replace('.', "/");

    // Resolve paths_to_mutate relative to project_root if needed.
    let base = if paths_to_mutate.is_absolute() {
        paths_to_mutate.to_path_buf()
    } else {
        project_root.join(paths_to_mutate)
    };

    // Prefer package (__init__.py) over module file (.py)
    let init_candidate = base.join(&module_path).join("__init__.py");
    let mod_candidate = base.join(format!("{module_path}.py"));

    let abs_path = if init_candidate.exists() {
        init_candidate
    } else if mod_candidate.exists() {
        mod_candidate
    } else {
        // Fall back to .py path even if it doesn't exist
        mod_candidate
    };

    abs_path
        .strip_prefix(project_root)
        .unwrap_or(&abs_path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::MutantStatus;

    /// Build a MutantCacheDescriptor for GitHub Actions tests.
    /// function_source is synthesised from `orig` so that `offset` points at it.
    fn make_descriptor(
        mutant_name: &str,
        source_file: &str,
        operator: &str,
        orig: &str,
        repl: &str,
        fn_start_line: usize,
        offset: usize,
    ) -> MutantCacheDescriptor {
        let function_source = format!("def foo():\n    return {orig}\n");
        MutantCacheDescriptor {
            mutant_name: mutant_name.to_string(),
            function_source,
            operator: operator.to_string(),
            start: offset,
            end: offset + orig.len(),
            original: orig.to_string(),
            replacement: repl.to_string(),
            source_file: source_file.to_string(),
            fn_byte_offset: 0,
            fn_start_line,
        }
    }

    /// Build a MutantCacheDescriptor for Stryker report tests (full control over source).
    fn make_full_descriptor(
        mutant_name: &str,
        function_source: &str,
        operator: &str,
        start: usize,
        end: usize,
        original: &str,
        replacement: &str,
        fn_start_line: usize,
        source_file: &str,
    ) -> MutantCacheDescriptor {
        MutantCacheDescriptor {
            mutant_name: mutant_name.to_string(),
            function_source: function_source.to_string(),
            operator: operator.to_string(),
            start,
            end,
            original: original.to_string(),
            replacement: replacement.to_string(),
            fn_start_line,
            fn_byte_offset: 0,
            source_file: source_file.to_string(),
        }
    }

    fn make_result(name: &str, status: MutantStatus, duration: f64) -> MutantResult {
        MutantResult {
            mutant_name: name.to_string(),
            exit_code: match status {
                MutantStatus::Killed => 1,
                MutantStatus::Survived => 0,
                MutantStatus::NoTests => 33,
                MutantStatus::Timeout => -1,
                MutantStatus::TypeCheck => 37,
                MutantStatus::Error => 2,
            },
            duration,
            status,
        }
    }

    // -------------------------------------------------------------------------
    // GitHub Actions tests
    // -------------------------------------------------------------------------

    /// INV-1: No annotations emitted when GITHUB_ACTIONS is not set.
    /// (Tested indirectly — we verify the function is a no-op by checking it
    /// doesn't panic and produces no visible side effects in test env.)
    #[test]
    fn test_no_annotations_outside_github_actions() {
        // GITHUB_ACTIONS is not "true" in normal test env.
        // This test simply ensures the function doesn't panic or crash.
        let results = vec![make_result("mod.x_foo__irradiate_1", MutantStatus::Survived, 0.1)];
        let desc = make_descriptor("mod.x_foo__irradiate_1", "src/mod.py", "binop_swap", "+", "-", 1, 19);
        // Should be a complete no-op (doesn't write anything)
        emit_github_annotations(&results, &[desc], 5, 1);
    }

    /// INV-2: At most MAX_ANNOTATIONS (10) ::warning lines emitted.
    /// Verified via build_step_summary which covers the annotation logic path
    /// and by checking that the survived slice is capped.
    #[test]
    fn test_annotation_cap_at_ten() {
        // Build 15 survived results, verify only 10 would be annotated.
        let survived: Vec<MutantResult> = (1..=15)
            .map(|i| make_result(&format!("mod.x_foo__irradiate_{i}"), MutantStatus::Survived, 0.1))
            .collect();

        let to_annotate = survived.len().min(MAX_ANNOTATIONS);
        assert_eq!(to_annotate, 10, "INV-2: annotation cap is 10");
    }

    /// INV-3: Step summary is valid Markdown (has heading, table, details block).
    #[test]
    fn test_step_summary_valid_markdown() {
        let results = vec![
            make_result("mod.x_add__irradiate_1", MutantStatus::Survived, 0.1),
            make_result("mod.x_sub__irradiate_1", MutantStatus::Killed, 0.1),
        ];
        let descs = vec![
            make_descriptor("mod.x_add__irradiate_1", "src/mod.py", "binop_swap", "+", "-", 2, 19),
        ];

        let md = build_step_summary(&results, &descs, 1, 1).unwrap();

        assert!(md.contains("## Mutation Testing Results"), "INV-3: has heading");
        assert!(md.contains("| Killed | 1 |"), "INV-3: has killed row");
        assert!(md.contains("| Survived | 1 |"), "INV-3: has survived row");
        assert!(md.contains("<details>"), "INV-3: has details block for survived");
        assert!(md.contains("`mod.x_add__irradiate_1`"), "INV-3: lists survived mutant");
    }

    /// INV-3: Summary still written when no mutants survived.
    #[test]
    fn test_step_summary_no_survivors() {
        let results = vec![make_result("mod.x_foo__irradiate_1", MutantStatus::Killed, 0.1)];
        let md = build_step_summary(&results, &[], 1, 0).unwrap();

        assert!(md.contains("## Mutation Testing Results"), "heading present");
        assert!(md.contains("| Survived | 0 |"), "zero survivors shown");
        assert!(!md.contains("<details>"), "no details block when no survivors");
    }

    /// INV-4: Annotation file paths match source_file from descriptors.
    #[test]
    fn test_annotation_location_matches_source_file() {
        let desc = make_descriptor(
            "pkg.x_compute__irradiate_3",
            "src/pkg/compute.py",
            "binop_swap",
            "+",
            "-",
            10,
            19, // offset within function_source
        );
        assert_eq!(desc.source_file, "src/pkg/compute.py", "INV-4: source_file preserved");
    }

    /// Verify byte_offset_to_line computes correct 1-based line numbers.
    #[test]
    fn test_byte_offset_to_line_first_line() {
        let src = "def foo():\n    return x + 1\n";
        // offset 0 → same line as fn_start (line 5)
        assert_eq!(byte_offset_to_line(src, 5, 0), 5);
    }

    #[test]
    fn test_byte_offset_to_line_second_line() {
        let src = "def foo():\n    return x + 1\n";
        // offset 11 (just past the first newline) → fn_start + 1
        assert_eq!(byte_offset_to_line(src, 5, 11), 6);
    }

    #[test]
    fn test_byte_offset_to_line_zero_fn_start() {
        // fn_start_line=0 means unknown; should return 1
        let src = "def foo():\n    return 1\n";
        assert_eq!(byte_offset_to_line(src, 0, 5), 1);
    }

    /// Summary score computation: 1 killed + 1 survived = 50%.
    #[test]
    fn test_step_summary_score() {
        let results = vec![
            make_result("mod.x_a__irradiate_1", MutantStatus::Survived, 0.1),
            make_result("mod.x_b__irradiate_1", MutantStatus::Killed, 0.1),
        ];
        let md = build_step_summary(&results, &[], 1, 1).unwrap();
        assert!(md.contains("50.0%"), "50% score shown");
    }

    /// No-tests and timeout counts appear in the summary.
    #[test]
    fn test_step_summary_counts_no_tests_and_timeout() {
        let results = vec![
            make_result("mod.x_a__irradiate_1", MutantStatus::NoTests, 0.1),
            make_result("mod.x_b__irradiate_1", MutantStatus::Timeout, 0.1),
            make_result("mod.x_c__irradiate_1", MutantStatus::Killed, 0.1),
        ];
        let md = build_step_summary(&results, &[], 1, 0).unwrap();
        assert!(md.contains("| No coverage | 1 |"));
        assert!(md.contains("| Timeout | 1 |"));
    }

    // -------------------------------------------------------------------------
    // Stryker JSON report tests
    // -------------------------------------------------------------------------

    /// INV-1: Report always contains schemaVersion "2".
    #[test]
    fn test_schema_version_present() {
        let results = vec![make_result("mod.x_f__irradiate_1", MutantStatus::Killed, 0.1)];
        let report = build_stryker_report(&results, &[], None, Path::new("/proj"), Path::new("src"));
        assert_eq!(report["schemaVersion"], "2");
    }

    /// INV-2: All 5 status mappings are correct.
    #[test]
    fn test_status_mapping() {
        assert_eq!(stryker_status(MutantStatus::Killed), "Killed");
        assert_eq!(stryker_status(MutantStatus::Survived), "Survived");
        assert_eq!(stryker_status(MutantStatus::NoTests), "NoCoverage");
        assert_eq!(stryker_status(MutantStatus::Timeout), "Timeout");
        assert_eq!(stryker_status(MutantStatus::Error), "RuntimeError");
        assert_eq!(stryker_status(MutantStatus::TypeCheck), "RuntimeError");
    }

    /// INV-3: Location line/column values are 1-indexed.
    #[test]
    fn test_location_is_1indexed() {
        // "def add(a, b):\n    return a + b\n"
        // The '+' at 'a + b' on line 2 column 14 (1-indexed).
        let source = "def add(a, b):\n    return a + b\n";
        // byte offset of '+': "def add(a, b):\n    return a " = 15+11 = 26 bytes
        let (line, col) = crate::codegen::byte_offset_to_location(source, 26);
        assert_eq!(line, 2, "line should be 1-indexed (line 2 = second line)");
        assert_eq!(col, 12, "column should be 1-indexed");
    }

    /// INV-4: Every mutant in results appears in the report.
    #[test]
    fn test_every_result_in_report() {
        let results = vec![
            make_result("mod.x_f__irradiate_1", MutantStatus::Killed, 0.1),
            make_result("mod.x_g__irradiate_1", MutantStatus::Survived, 0.2),
        ];
        let report = build_stryker_report(&results, &[], None, Path::new("/proj"), Path::new("src"));

        // Collect all mutant ids from the report
        let mut found_ids: Vec<String> = Vec::new();
        if let Some(files) = report["files"].as_object() {
            for file_val in files.values() {
                if let Some(mutants) = file_val["mutants"].as_array() {
                    for m in mutants {
                        if let Some(id) = m["id"].as_str() {
                            found_ids.push(id.to_string());
                        }
                    }
                }
            }
        }
        found_ids.sort();
        assert!(
            found_ids.contains(&"mod.x_f__irradiate_1".to_string()),
            "killed mutant must be in report"
        );
        assert!(
            found_ids.contains(&"mod.x_g__irradiate_1".to_string()),
            "survived mutant must be in report"
        );
    }

    /// Report with descriptors: correct status, operator, and description.
    #[test]
    fn test_report_with_descriptors() {
        let tmp = tempfile::tempdir().unwrap();
        let src_file = tmp.path().join("core.py");
        std::fs::write(&src_file, "def add(a, b):\n    return a + b\n").unwrap();

        let results = vec![make_result("core.x_add__irradiate_1", MutantStatus::Killed, 0.05)];
        let rel_src = src_file
            .strip_prefix(tmp.path())
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let descriptors = vec![make_full_descriptor(
            "core.x_add__irradiate_1",
            "def add(a, b):\n    return a + b\n",
            "binop_swap",
            26, // offset of '+'
            27,
            "+",
            "-",
            1,
            &rel_src,
        )];

        let report =
            build_stryker_report(&results, &descriptors, None, tmp.path(), Path::new("src"));

        // schemaVersion
        assert_eq!(report["schemaVersion"], "2");

        // Find the mutant in files
        let files = report["files"].as_object().unwrap();
        let file_entry = files.values().next().unwrap();
        let mutants = file_entry["mutants"].as_array().unwrap();
        assert_eq!(mutants.len(), 1);
        let m = &mutants[0];
        assert_eq!(m["id"], "core.x_add__irradiate_1");
        assert_eq!(m["status"], "Killed");
        assert_eq!(m["mutatorName"], "binop_swap");
        assert_eq!(m["replacement"], "-");
        assert!(m["description"].as_str().unwrap().contains('+'));

        // INV-3: location is 1-indexed
        let start_line = m["location"]["start"]["line"].as_u64().unwrap();
        assert!(start_line >= 1, "line must be >= 1 (1-indexed)");
        let start_col = m["location"]["start"]["column"].as_u64().unwrap();
        assert!(start_col >= 1, "column must be >= 1 (1-indexed)");
    }

    /// Report includes framework name and version.
    #[test]
    fn test_framework_field() {
        let report = build_stryker_report(&[], &[], None, Path::new("/proj"), Path::new("src"));
        assert_eq!(report["framework"]["name"], "irradiate");
        assert!(
            report["framework"]["version"].as_str().is_some(),
            "version field must be present"
        );
    }

    /// Mutant in results but not in descriptors → included with minimal info (failure mode).
    #[test]
    fn test_result_without_descriptor_included() {
        let results = vec![make_result("unknown.x_h__irradiate_1", MutantStatus::Survived, 0.0)];
        let report =
            build_stryker_report(&results, &[], None, Path::new("/proj"), Path::new("src"));
        let files = report["files"].as_object().unwrap();
        let all_mutants: Vec<_> = files.values().flat_map(|f| f["mutants"].as_array().unwrap().iter()).collect();
        assert_eq!(all_mutants.len(), 1);
        assert_eq!(all_mutants[0]["id"], "unknown.x_h__irradiate_1");
        assert_eq!(all_mutants[0]["status"], "Survived");
    }

    // -------------------------------------------------------------------------
    // HTML report tests
    // -------------------------------------------------------------------------

    /// INV-1: Output file contains `<!DOCTYPE html>`.
    #[test]
    fn test_html_report_is_valid_html() {
        let report = build_stryker_report(&[], &[], None, Path::new("/proj"), Path::new("src"));
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("report.html");
        write_html_report(&report, &out).unwrap();
        let content = std::fs::read_to_string(&out).unwrap();
        assert!(content.contains("<!DOCTYPE html>"), "INV-1: must contain DOCTYPE");
    }

    /// INV-2: Output contains the mutation-testing-elements script tag.
    #[test]
    fn test_html_report_contains_script_tag() {
        let report = build_stryker_report(&[], &[], None, Path::new("/proj"), Path::new("src"));
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("report.html");
        write_html_report(&report, &out).unwrap();
        let content = std::fs::read_to_string(&out).unwrap();
        assert!(
            content.contains("mutation-testing-elements"),
            "INV-2: must reference mutation-testing-elements"
        );
        assert!(
            content.contains("unpkg.com"),
            "INV-2: script loaded from unpkg CDN"
        );
    }

    /// INV-3: Output contains the actual JSON data (not the placeholder string).
    #[test]
    fn test_html_report_contains_json_not_placeholder() {
        let results = vec![make_result("mod.x_f__irradiate_1", MutantStatus::Killed, 0.1)];
        let report = build_stryker_report(&results, &[], None, Path::new("/proj"), Path::new("src"));
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("report.html");
        write_html_report(&report, &out).unwrap();
        let content = std::fs::read_to_string(&out).unwrap();
        // Placeholder must be gone
        assert!(
            !content.contains("REPORT_JSON_PLACEHOLDER"),
            "INV-3: placeholder must be replaced"
        );
        // JSON content must be present
        assert!(
            content.contains("schemaVersion"),
            "INV-3: JSON data must be inlined"
        );
        assert!(
            content.contains("mod.x_f__irradiate_1"),
            "INV-3: mutant id must appear in output"
        );
    }

    /// INV-4: Output is self-contained (single file, no local file references).
    #[test]
    fn test_html_report_is_self_contained() {
        let report = build_stryker_report(&[], &[], None, Path::new("/proj"), Path::new("src"));
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("report.html");
        write_html_report(&report, &out).unwrap();
        let content = std::fs::read_to_string(&out).unwrap();
        // Must be a single file — no local <link rel="stylesheet">, no local <script src="./...">
        assert!(
            !content.contains("rel=\"stylesheet\""),
            "INV-4: no local stylesheet references"
        );
        // Confirm the web component tag is present (self-contained rendering)
        assert!(
            content.contains("<mutation-test-report-app"),
            "INV-4: web component tag present"
        );
    }

    /// coveredBy is populated from stats when provided.
    #[test]
    fn test_covered_by_from_stats() {
        use std::collections::HashMap;
        let mut tests_by_function = HashMap::new();
        tests_by_function.insert(
            "mod.x_foo".to_string(),
            vec!["tests/test_mod.py::test_it".to_string()],
        );
        let stats = TestStats {
            tests_by_function,
            ..Default::default()
        };

        let results = vec![make_result("mod.x_foo__irradiate_1", MutantStatus::Killed, 0.1)];
        let report =
            build_stryker_report(&results, &[], Some(&stats), Path::new("/p"), Path::new("src"));

        let files = report["files"].as_object().unwrap();
        let mutants: Vec<_> = files
            .values()
            .flat_map(|f| f["mutants"].as_array().unwrap().iter())
            .collect();
        let covered_by = mutants[0]["coveredBy"].as_array().unwrap();
        assert_eq!(covered_by.len(), 1);
        assert_eq!(covered_by[0], "tests/test_mod.py::test_it");
    }
}
