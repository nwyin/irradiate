use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::process::Command;

pub type LineRange = RangeInclusive<usize>;

/// Maps file paths to changed line ranges.
/// None = new file (all lines changed, mutate everything).
/// Some(ranges) = specific changed line ranges on the new side.
#[derive(Debug, Default)]
pub struct DiffFilter {
    pub changed_files: HashMap<PathBuf, Option<Vec<LineRange>>>,
}

impl DiffFilter {
    /// Check if a file has any changes in the diff.
    pub fn file_is_changed(&self, rel_path: &Path) -> bool {
        self.changed_files.contains_key(rel_path)
    }

    /// Check if a function spanning [start_line, end_line] (1-indexed, inclusive)
    /// overlaps with any changed line range. Returns true for new files (None ranges).
    pub fn function_is_touched(&self, rel_path: &Path, start_line: usize, end_line: usize) -> bool {
        match self.changed_files.get(rel_path) {
            None => false,                    // file not in diff
            Some(None) => true,              // new file — everything touched
            Some(Some(ranges)) => ranges.iter().any(|r| *r.start() <= end_line && *r.end() >= start_line),
        }
    }
}

/// Run git diff against a reference and parse the output.
/// Uses merge-base resolution: `--diff main` gives "changes since branch point",
/// not "diff against main tip".
pub fn parse_git_diff(diff_ref: &str, repo_root: &Path) -> anyhow::Result<DiffFilter> {
    // Try merge-base first to get the branch divergence point.
    let merge_base = Command::new("git")
        .args(["merge-base", diff_ref, "HEAD"])
        .current_dir(repo_root)
        .output()?;

    let effective_ref = if merge_base.status.success() {
        String::from_utf8_lossy(&merge_base.stdout).trim().to_string()
    } else {
        diff_ref.to_string()
    };

    let output = Command::new("git")
        .args(["diff", "--no-color", "-U0", "--diff-filter=ACMR", &effective_ref])
        .current_dir(repo_root)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git diff failed for ref '{}': {}", diff_ref, stderr.trim());
    }

    let diff_text = String::from_utf8_lossy(&output.stdout);
    parse_unified_diff(&diff_text)
}

/// Parse unified diff text into a DiffFilter. Public for testing.
pub fn parse_unified_diff(diff_output: &str) -> anyhow::Result<DiffFilter> {
    let mut filter = DiffFilter::default();
    let mut current_file: Option<PathBuf> = None;
    let mut is_new_file = false;

    for line in diff_output.lines() {
        if line.starts_with("diff --git ") {
            // Finalize previous file.
            if let Some(path) = current_file.take() {
                if is_new_file {
                    filter.changed_files.entry(path).or_insert(None);
                } else {
                    filter.changed_files.entry(path).or_insert_with(|| Some(vec![]));
                }
            }
            // Extract the b/ path.
            current_file = parse_diff_git_header(line);
            is_new_file = false;
        } else if line.starts_with("new file mode") {
            is_new_file = true;
        } else if line.starts_with("@@ ") {
            if let Some(path) = &current_file {
                if is_new_file {
                    // New file — entry stays as None (mutate everything).
                    filter.changed_files.entry(path.clone()).or_insert(None);
                } else if let Some(range) = parse_hunk_header(line) {
                    filter
                        .changed_files
                        .entry(path.clone())
                        .and_modify(|v| {
                            if let Some(ranges) = v {
                                ranges.push(range.clone());
                            }
                        })
                        .or_insert_with(|| Some(vec![range]));
                }
                // count=0 (pure deletion) → parse_hunk_header returns None, nothing inserted.
            }
        }
    }

    // Finalize last file.
    if let Some(path) = current_file {
        if is_new_file {
            filter.changed_files.entry(path).or_insert(None);
        } else {
            filter.changed_files.entry(path).or_insert_with(|| Some(vec![]));
        }
    }

    Ok(filter)
}

/// Extract the `b/` file path from a `diff --git a/path b/path` header line.
fn parse_diff_git_header(line: &str) -> Option<PathBuf> {
    // Format: diff --git a/<path> b/<path>
    // The b/ path is the new-side path we want.
    let rest = line.strip_prefix("diff --git ")?;
    // Find " b/" — split on last occurrence to handle spaces in filenames.
    let b_idx = rest.rfind(" b/")?;
    let b_path = &rest[b_idx + 3..]; // skip " b/"
    Some(PathBuf::from(b_path))
}

/// Parse a `@@ -old +new @@` hunk header and return the new-side line range.
/// Returns None for pure deletions (count = 0) or unparseable headers.
pub(crate) fn parse_hunk_header(line: &str) -> Option<LineRange> {
    // Format: @@ -a[,b] +c[,d] @@ ...
    // We care only about the +c[,d] part (new side).
    let inner = line.strip_prefix("@@ ")?;
    // Find the '+' that starts the new-side range.
    let plus_idx = inner.find(" +")?;
    let after_plus = &inner[plus_idx + 2..];
    // Trim everything after the closing " @@" or end of relevant tokens.
    let token = after_plus.split_whitespace().next()?;

    if let Some((start_str, count_str)) = token.split_once(',') {
        let start: usize = start_str.parse().ok()?;
        let count: usize = count_str.parse().ok()?;
        if count == 0 {
            return None; // pure deletion
        }
        Some(start..=start + count - 1)
    } else {
        // No comma — single line (count implicitly 1)
        let start: usize = token.parse().ok()?;
        Some(start..=start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_hunk_header ---

    #[test]
    fn hunk_standard_range() {
        // @@ -1,5 +10,8 @@ — lines 10..17
        let range = parse_hunk_header("@@ -1,5 +10,8 @@ some context").unwrap();
        assert_eq!(range, 10..=17);
    }

    #[test]
    fn hunk_single_line_no_comma() {
        // @@ -a,b +5 @@ — single new line at 5
        let range = parse_hunk_header("@@ -1,3 +5 @@").unwrap();
        assert_eq!(range, 5..=5);
    }

    #[test]
    fn hunk_pure_deletion_returns_none() {
        // INV-2: count=0 → None
        let result = parse_hunk_header("@@ -5,3 +5,0 @@");
        assert!(result.is_none(), "pure deletion should return None");
    }

    #[test]
    fn hunk_old_side_no_comma() {
        // @@ -a +c,d @@
        let range = parse_hunk_header("@@ -3 +7,4 @@").unwrap();
        assert_eq!(range, 7..=10);
    }

    // --- parse_unified_diff ---

    #[test]
    fn new_file_has_none_ranges() {
        // INV-1: new files → None (mutate everything)
        let diff = "\
diff --git a/src/foo.py b/src/foo.py
new file mode 100644
index 0000000..abc1234
--- /dev/null
+++ b/src/foo.py
@@ -0,0 +1,10 @@
+def hello():
+    pass
";
        let filter = parse_unified_diff(diff).unwrap();
        let path = PathBuf::from("src/foo.py");
        assert!(filter.file_is_changed(&path));
        // INV-1: value must be None for new file
        assert!(filter.changed_files[&path].is_none());
        // function_is_touched must return true for any span
        assert!(filter.function_is_touched(&path, 1, 10));
        assert!(filter.function_is_touched(&path, 999, 1000));
    }

    #[test]
    fn modified_file_collects_hunk_ranges() {
        let diff = "\
diff --git a/src/bar.py b/src/bar.py
index abc..def 100644
--- a/src/bar.py
+++ b/src/bar.py
@@ -3,2 +3,3 @@ def bar():
+    extra = 1
 unchanged
 line
@@ -20,1 +21,2 @@
+    another
 line
";
        let filter = parse_unified_diff(diff).unwrap();
        let path = PathBuf::from("src/bar.py");
        assert!(filter.file_is_changed(&path));
        let ranges = filter.changed_files[&path].as_ref().unwrap();
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0], 3..=5);
        assert_eq!(ranges[1], 21..=22);
    }

    #[test]
    fn pure_deletion_hunk_produces_no_range() {
        // INV-2: pure deletion hunk adds no entry for line ranges
        let diff = "\
diff --git a/src/baz.py b/src/baz.py
index abc..def 100644
--- a/src/baz.py
+++ b/src/baz.py
@@ -5,3 +5,0 @@
-    deleted_line_1
-    deleted_line_2
-    deleted_line_3
";
        let filter = parse_unified_diff(diff).unwrap();
        let path = PathBuf::from("src/baz.py");
        assert!(filter.file_is_changed(&path));
        // File is present but with empty ranges (pure deletion)
        let ranges = filter.changed_files[&path].as_ref().unwrap();
        assert!(ranges.is_empty());
    }

    #[test]
    fn function_not_touched_for_unknown_file() {
        // INV-3: file not in diff → false
        let filter = parse_unified_diff("").unwrap();
        assert!(!filter.function_is_touched(Path::new("unknown.py"), 1, 100));
    }

    #[test]
    fn function_overlap_boundary_conditions() {
        // INV-4: overlap check correctness
        let diff = "\
diff --git a/a.py b/a.py
index 0..1 100644
--- a/a.py
+++ b/a.py
@@ -1,5 +15,11 @@
 changed
";
        let filter = parse_unified_diff(diff).unwrap();
        let path = PathBuf::from("a.py");
        // Range is 15..=25

        // Overlapping: function [10,20] ∩ [15,25]
        assert!(filter.function_is_touched(&path, 10, 20));
        // Non-overlapping: function [26,30] — comes after [15,25]
        assert!(!filter.function_is_touched(&path, 26, 30));
        // Adjacent but not overlapping: [1,14]
        assert!(!filter.function_is_touched(&path, 1, 14));
        // Exactly touching start: [14,15] overlaps
        assert!(filter.function_is_touched(&path, 14, 15));
        // Exactly touching end: [25,30] overlaps
        assert!(filter.function_is_touched(&path, 25, 30));
        // Fully inside: [17,20]
        assert!(filter.function_is_touched(&path, 17, 20));
        // Wraps around: [1,30]
        assert!(filter.function_is_touched(&path, 1, 30));
    }

    #[test]
    fn multi_file_diff_parsed_correctly() {
        let diff = "\
diff --git a/src/new.py b/src/new.py
new file mode 100644
index 0000000..111
--- /dev/null
+++ b/src/new.py
@@ -0,0 +1,5 @@
+content
diff --git a/src/modified.py b/src/modified.py
index 000..111 100644
--- a/src/modified.py
+++ b/src/modified.py
@@ -10,3 +10,4 @@
 unchanged
diff --git a/src/deleted.py b/src/deleted.py
deleted file mode 100644
index 111..000
--- a/src/deleted.py
+++ /dev/null
";
        let filter = parse_unified_diff(diff).unwrap();

        // New file → None ranges
        assert!(filter.changed_files[&PathBuf::from("src/new.py")].is_none());
        // Modified file → has ranges
        let modified = filter.changed_files[&PathBuf::from("src/modified.py")].as_ref().unwrap();
        assert_eq!(modified.len(), 1);
        assert_eq!(modified[0], 10..=13);
    }

    #[test]
    fn empty_diff_returns_empty_filter() {
        let filter = parse_unified_diff("").unwrap();
        assert!(filter.changed_files.is_empty());
    }

    #[test]
    fn binary_file_in_diff_has_empty_ranges() {
        // Binary files appear in diff header but have no hunk headers.
        let diff = "\
diff --git a/image.png b/image.png
index abc..def 100644
Binary files a/image.png and b/image.png differ
";
        let filter = parse_unified_diff(diff).unwrap();
        let path = PathBuf::from("image.png");
        assert!(filter.file_is_changed(&path));
        // No hunks → empty range list (Some([]))
        let ranges = filter.changed_files[&path].as_ref().unwrap();
        assert!(ranges.is_empty());
    }
}
