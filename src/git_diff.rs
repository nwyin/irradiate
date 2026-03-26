//! Git diff parsing for incremental (diff-aware) mutation testing.
//!
//! Parses `git diff <ref>` unified diff output to determine which lines changed.
//! The `DiffFilter` can then tell codegen which functions overlap with changed lines.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A half-open range of line numbers [start, end] (both 1-indexed, inclusive).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineRange {
    pub start: usize,
    pub end: usize,
}

impl LineRange {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// Returns true if this range overlaps with [other_start, other_end] (inclusive).
    pub fn overlaps(&self, other_start: usize, other_end: usize) -> bool {
        self.start <= other_end && self.end >= other_start
    }
}

/// Holds the parsed diff result: which files changed and which line ranges were touched.
///
/// - `None` value → new file (all lines are "changed", mutate everything)
/// - `Some([])` → file was deleted or only binary/mode changes (no mutatable lines)
/// - `Some(ranges)` → modified file with specific changed hunks
#[derive(Debug, Default)]
pub struct DiffFilter {
    /// Map from repo-relative file path to changed line ranges (in the new version).
    files: HashMap<PathBuf, Option<Vec<LineRange>>>,
}

impl DiffFilter {
    /// Returns true if the function spanning [fn_start_line, fn_end_line] is touched by the diff.
    ///
    /// A function is "touched" if:
    /// - The file it lives in was newly added (None entry), OR
    /// - Any changed hunk overlaps the function's line span
    pub fn function_is_touched(&self, rel_path: &Path, fn_start_line: usize, fn_end_line: usize) -> bool {
        match self.files.get(rel_path) {
            None => false, // file not in diff at all
            Some(None) => true, // new file — all functions are "touched"
            Some(Some(ranges)) => {
                ranges.iter().any(|r| r.overlaps(fn_start_line, fn_end_line))
            }
        }
    }

    /// Returns true if the given file path appears anywhere in the diff.
    pub fn file_is_touched(&self, rel_path: &Path) -> bool {
        self.files.contains_key(rel_path)
    }

    /// Returns the number of files in the diff.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Returns the total number of functions that could be touched (rough upper bound for display).
    /// This counts distinct files that have at least one change range (or are new).
    pub fn changed_file_count(&self) -> usize {
        self.files
            .values()
            .filter(|v| matches!(v, None | Some(_)))
            .count()
    }
}

/// Find the root of the current git repository by running `git rev-parse --show-toplevel`.
pub fn find_git_root(start: &Path) -> Result<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(start)
        .output()
        .context("failed to run git — is git installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("not inside a git repository: {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(PathBuf::from(stdout.trim()))
}

/// Try to resolve the merge-base between `diff_ref` and HEAD.
///
/// Returns `Some(sha)` if merge-base succeeds, `None` if it fails (e.g. unrelated
/// histories, detached HEAD, or `diff_ref` is already a commit SHA / relative ref).
fn resolve_merge_base(diff_ref: &str, repo_root: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["merge-base", diff_ref, "HEAD"])
        .current_dir(repo_root)
        .output()
        .ok()?;

    if output.status.success() {
        let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !sha.is_empty() {
            return Some(sha);
        }
    }
    None
}

/// Run `git diff <diff_ref>` and parse the unified diff output into a `DiffFilter`.
///
/// `diff_ref` can be any git ref: "main", "HEAD~3", a commit SHA, etc.
/// When `diff_ref` is a branch name, we resolve it via `git merge-base` first
/// so that `--diff main` means "changes since this branch diverged from main"
/// rather than "diff between working tree and main's current tip".
pub fn parse_git_diff(diff_ref: &str, repo_root: &Path) -> Result<DiffFilter> {
    // Resolve merge-base so branch names compare against the divergence point.
    let effective_ref = resolve_merge_base(diff_ref, repo_root).unwrap_or_else(|| diff_ref.to_string());

    let output = std::process::Command::new("git")
        .args(["diff", &effective_ref, "--unified=0", "--no-color"])
        .current_dir(repo_root)
        .output()
        .context("failed to run git diff")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "git diff '{}' failed: {}\nVerify the ref exists: git rev-parse --verify {}",
            diff_ref,
            stderr.trim(),
            diff_ref
        );
    }

    let diff_text = String::from_utf8_lossy(&output.stdout);
    Ok(parse_unified_diff(&diff_text))
}

/// Parse a unified diff text (from `git diff --unified=0`) into a `DiffFilter`.
///
/// We only care about Python files and only track lines added/modified in the new file (+++ side).
pub fn parse_unified_diff(diff: &str) -> DiffFilter {
    let mut filter = DiffFilter::default();
    let mut current_file: Option<PathBuf> = None;
    let mut current_ranges: Vec<LineRange> = Vec::new();
    let mut is_new_file = false;

    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            // Finalize the previous file.
            if let Some(path) = current_file.take() {
                if is_new_file {
                    filter.files.insert(path, None);
                } else {
                    filter.files.insert(path, Some(current_ranges.clone()));
                }
            }
            current_ranges.clear();
            is_new_file = false;
        } else if let Some(path_str) = line.strip_prefix("+++ b/") {
            // New file path — stripped the "+++ b/" prefix.
            current_file = if path_str == "/dev/null" {
                None // deleted file
            } else {
                Some(PathBuf::from(path_str))
            };
        } else if line == "+++ /dev/null" || line.starts_with("+++ /dev/null") {
            // File was deleted.
            current_file = None;
        } else if line.starts_with("new file mode") {
            is_new_file = true;
        } else if line.starts_with("@@ ") {
            // Parse hunk header: @@ -OLD_START[,OLD_LEN] +NEW_START[,NEW_LEN] @@
            if let Some(range) = parse_hunk_header(line) {
                // For a new file, we track ranges too (they cover the whole file).
                // The None-entry (new file) in DiffFilter means "mutate all", but we still
                // want to be able to answer function_is_touched correctly via ranges.
                // Actually, we handle new files via is_new_file flag → None entry, which
                // means function_is_touched returns true for all functions. No need to track ranges.
                if !is_new_file {
                    current_ranges.push(range);
                }
            }
        }
    }

    // Finalize the last file.
    if let Some(path) = current_file.take() {
        if is_new_file {
            filter.files.insert(path, None);
        } else {
            filter.files.insert(path, Some(current_ranges));
        }
    }

    filter
}

/// Parse a unified diff hunk header like `@@ -10,5 +12,7 @@` or `@@ -10 +12 @@`.
///
/// Returns a `LineRange` for the new (+) side of the hunk.
/// Returns `None` if the hunk adds no lines (pure deletion).
fn parse_hunk_header(line: &str) -> Option<LineRange> {
    // Format: @@ -OLD_START[,OLD_LEN] +NEW_START[,NEW_LEN] @@[ context]
    let after_at = line.strip_prefix("@@ ")?;
    let parts: Vec<&str> = after_at.split_whitespace().collect();
    // parts[0] = "-OLD_START[,OLD_LEN]", parts[1] = "+NEW_START[,NEW_LEN]"
    let new_part = parts.get(1)?;
    let new_part = new_part.strip_prefix('+')?;

    let (start, len) = if let Some((s, l)) = new_part.split_once(',') {
        let start: usize = s.parse().ok()?;
        let len: usize = l.parse().ok()?;
        (start, len)
    } else {
        let start: usize = new_part.parse().ok()?;
        (start, 1)
    };

    if len == 0 {
        // Pure deletion hunk — no lines in new file.
        return None;
    }

    Some(LineRange::new(start, start + len - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hunk_header_basic() {
        let r = parse_hunk_header("@@ -10,5 +12,7 @@ def foo():").unwrap();
        assert_eq!(r.start, 12);
        assert_eq!(r.end, 18); // 12 + 7 - 1
    }

    #[test]
    fn test_parse_hunk_header_single_line() {
        let r = parse_hunk_header("@@ -5 +5 @@").unwrap();
        assert_eq!(r.start, 5);
        assert_eq!(r.end, 5);
    }

    #[test]
    fn test_parse_hunk_header_pure_deletion() {
        // +0,0 means no lines added
        assert!(parse_hunk_header("@@ -5,3 +5,0 @@").is_none());
    }

    #[test]
    fn test_line_range_overlaps() {
        let r = LineRange::new(10, 20);
        assert!(r.overlaps(15, 25)); // overlaps at end
        assert!(r.overlaps(5, 12));  // overlaps at start
        assert!(r.overlaps(12, 18)); // contained
        assert!(r.overlaps(5, 25));  // contains
        assert!(r.overlaps(10, 10)); // boundary start
        assert!(r.overlaps(20, 20)); // boundary end
        assert!(!r.overlaps(21, 30)); // after
        assert!(!r.overlaps(1, 9));   // before
    }

    #[test]
    fn test_parse_unified_diff_modified_file() {
        let diff = "\
diff --git a/src/foo.py b/src/foo.py
index abc..def 100644
--- a/src/foo.py
+++ b/src/foo.py
@@ -10,3 +10,5 @@ def old():
-old line
+new line1
+new line2
@@ -50,1 +52,1 @@
-x = 1
+x = 2
";
        let filter = parse_unified_diff(diff);
        let path = PathBuf::from("src/foo.py");
        assert!(filter.file_is_touched(&path));

        let ranges = filter.files[&path].as_ref().unwrap();
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0], LineRange::new(10, 14));
        assert_eq!(ranges[1], LineRange::new(52, 52));
    }

    #[test]
    fn test_parse_unified_diff_new_file() {
        let diff = "\
diff --git a/src/new.py b/src/new.py
new file mode 100644
index 000..abc
--- /dev/null
+++ b/src/new.py
@@ -0,0 +1,10 @@
+def foo():
+    pass
";
        let filter = parse_unified_diff(diff);
        let path = PathBuf::from("src/new.py");
        assert!(filter.file_is_touched(&path));
        // None = new file, all functions touched
        assert!(filter.files[&path].is_none());
        assert!(filter.function_is_touched(&path, 1, 5));
    }

    #[test]
    fn test_parse_unified_diff_last_file_finalized() {
        // Regression: the last file in a diff must be finalized after the loop ends.
        let diff = "\
diff --git a/src/a.py b/src/a.py
index abc..def 100644
--- a/src/a.py
+++ b/src/a.py
@@ -1,1 +1,1 @@
-old
+new
diff --git a/src/b.py b/src/b.py
index abc..def 100644
--- a/src/b.py
+++ b/src/b.py
@@ -5,1 +5,1 @@
-old
+new
";
        let filter = parse_unified_diff(diff);
        assert!(filter.file_is_touched(&PathBuf::from("src/a.py")));
        assert!(filter.file_is_touched(&PathBuf::from("src/b.py")));
    }

    #[test]
    fn test_function_is_touched_no_overlap() {
        let diff = "\
diff --git a/src/foo.py b/src/foo.py
index abc..def 100644
--- a/src/foo.py
+++ b/src/foo.py
@@ -1,5 +1,5 @@ def foo():
-old
+new
";
        let filter = parse_unified_diff(diff);
        let path = PathBuf::from("src/foo.py");
        // Function at lines 10-20 is not touched by hunk at lines 1-5.
        assert!(!filter.function_is_touched(&path, 10, 20));
        // Function at lines 1-5 is touched.
        assert!(filter.function_is_touched(&path, 1, 5));
    }

    #[test]
    fn test_function_is_touched_untouched_file() {
        let diff = "\
diff --git a/src/foo.py b/src/foo.py
index abc..def 100644
--- a/src/foo.py
+++ b/src/foo.py
@@ -1,1 +1,1 @@
-old
+new
";
        let filter = parse_unified_diff(diff);
        // bar.py is not in the diff at all.
        assert!(!filter.function_is_touched(&PathBuf::from("src/bar.py"), 1, 100));
    }

    #[test]
    fn test_parse_unified_diff_multiple_files() {
        let diff = "\
diff --git a/src/a.py b/src/a.py
index abc..def 100644
--- a/src/a.py
+++ b/src/a.py
@@ -10,3 +10,3 @@
-old
+new
diff --git a/src/b.py b/src/b.py
new file mode 100644
index 000..abc
--- /dev/null
+++ b/src/b.py
@@ -0,0 +1,5 @@
+new content
";
        let filter = parse_unified_diff(diff);

        let a = PathBuf::from("src/a.py");
        let b = PathBuf::from("src/b.py");

        // a.py is modified
        assert!(filter.files[&a].as_ref().is_some());
        // b.py is new
        assert!(filter.files[&b].is_none());
    }
}
