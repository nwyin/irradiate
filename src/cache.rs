use crate::protocol::MutantStatus;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const CACHE_SCHEMA_VERSION: u32 = 1;
pub const CACHE_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone)]
pub struct MutantCacheDescriptor {
    pub mutant_name: String,
    pub function_source: String,
    pub operator: String,
    pub start: usize,
    pub end: usize,
    pub original: String,
    pub replacement: String,
    /// Path to the source file, relative to the project root (e.g. `src/mylib/core.py`).
    pub source_file: String,
    /// Byte offset of the function definition start within the source file.
    /// `fn_byte_offset + start` gives the absolute byte position of the mutation in the file.
    pub fn_byte_offset: usize,
    /// 1-indexed start line of the function in the source file. 0 if unknown.
    pub fn_start_line: usize,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CacheCounts {
    pub hits: usize,
    pub misses: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CacheEntry {
    pub schema_version: u32,
    pub exit_code: i32,
    pub duration: f64,
    pub status: MutantStatus,
}

/// Result of a garbage collection run.
#[derive(Debug)]
pub struct GcResult {
    /// Number of entries that were (or would be) pruned.
    pub pruned: usize,
    /// Total bytes freed (or that would be freed).
    pub pruned_bytes: u64,
    /// Number of entries remaining after GC.
    pub remaining: usize,
    /// Total bytes remaining after GC.
    pub remaining_bytes: u64,
}

pub fn cache_dir(project_dir: &Path) -> PathBuf {
    project_dir.join(".irradiate").join("cache")
}

/// Parse a human-readable duration string into seconds.
///
/// Supported formats: `30d`, `7d`, `24h`, `1h30m`, `90m`, `3600s`.
/// Multiple units can be combined: `1d12h`, `2h30m`.
pub fn parse_duration(s: &str) -> Result<u64> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("empty duration string");
    }
    let mut total_secs: u64 = 0;
    let mut num_buf = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            num_buf.push(ch);
        } else {
            if num_buf.is_empty() {
                anyhow::bail!("invalid duration format: '{s}' (expected number before '{ch}')");
            }
            let n: u64 = num_buf.parse().with_context(|| format!("invalid number in duration: '{s}'"))?;
            num_buf.clear();
            let multiplier = match ch {
                'd' => 86400,
                'h' => 3600,
                'm' => 60,
                's' => 1,
                _ => anyhow::bail!("invalid duration unit '{ch}' in '{s}' (expected d/h/m/s)"),
            };
            total_secs += n * multiplier;
        }
    }
    if !num_buf.is_empty() {
        anyhow::bail!("invalid duration format: '{s}' (trailing number without unit)");
    }
    Ok(total_secs)
}

/// Parse a human-readable size string into bytes.
///
/// Supported formats: `500mb`, `1gb`, `100kb`, `1024b`.
/// Case-insensitive.
pub fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        anyhow::bail!("empty size string");
    }
    // Split into numeric prefix and unit suffix
    let num_end = s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len());
    if num_end == 0 {
        anyhow::bail!("invalid size format: '{s}' (expected number)");
    }
    let num_str = &s[..num_end];
    let unit = &s[num_end..];
    let n: f64 = num_str.parse().with_context(|| format!("invalid number in size: '{s}'"))?;
    let multiplier: u64 = match unit {
        "b" | "" => 1,
        "kb" | "k" => 1024,
        "mb" | "m" => 1024 * 1024,
        "gb" | "g" => 1024 * 1024 * 1024,
        _ => anyhow::bail!("invalid size unit '{unit}' in '{s}' (expected b/kb/mb/gb)"),
    };
    Ok((n * multiplier as f64) as u64)
}

/// Run garbage collection on the cache directory.
///
/// Prunes entries older than `max_age_secs` (by mtime), then evicts oldest entries
/// until total size is under `max_size_bytes`. If `dry_run` is true, no files are deleted.
pub fn gc(project_dir: &Path, max_age_secs: u64, max_size_bytes: u64, dry_run: bool) -> Result<GcResult> {
    let dir = cache_dir(project_dir);
    if !dir.exists() {
        return Ok(GcResult { pruned: 0, pruned_bytes: 0, remaining: 0, remaining_bytes: 0 });
    }

    let now = SystemTime::now();

    // Collect all cache entries with their metadata
    let mut entries: Vec<(PathBuf, u64, SystemTime)> = Vec::new(); // (path, size, mtime)
    for bucket in fs::read_dir(&dir).with_context(|| format!("Failed to read {}", dir.display()))? {
        let bucket = bucket?;
        if !bucket.file_type()?.is_dir() {
            continue;
        }
        for entry in fs::read_dir(bucket.path())? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                let meta = entry.metadata()?;
                entries.push((path, meta.len(), meta.modified().unwrap_or(UNIX_EPOCH)));
            }
        }
    }

    let mut pruned: usize = 0;
    let mut pruned_bytes: u64 = 0;
    let mut kept: Vec<(PathBuf, u64, SystemTime)> = Vec::new();

    // Phase 1: Age-based pruning
    for (path, size, mtime) in entries {
        let age = now.duration_since(mtime).unwrap_or_default();
        if age.as_secs() > max_age_secs {
            pruned += 1;
            pruned_bytes += size;
            if !dry_run {
                let _ = fs::remove_file(&path);
            }
        } else {
            kept.push((path, size, mtime));
        }
    }

    // Phase 2: Size-based eviction (oldest first)
    let total_kept_bytes: u64 = kept.iter().map(|(_, s, _)| s).sum();
    if total_kept_bytes > max_size_bytes {
        // Sort oldest first (ascending mtime)
        kept.sort_by_key(|(_, _, mtime)| *mtime);
        let mut current_bytes = total_kept_bytes;
        let mut still_kept = Vec::new();
        for (path, size, mtime) in kept {
            if current_bytes > max_size_bytes {
                pruned += 1;
                pruned_bytes += size;
                current_bytes -= size;
                if !dry_run {
                    let _ = fs::remove_file(&path);
                }
            } else {
                still_kept.push((path, size, mtime));
            }
        }
        kept = still_kept;
    }

    // Clean up empty bucket directories
    if !dry_run {
        if let Ok(buckets) = fs::read_dir(&dir) {
            for bucket in buckets.flatten() {
                if bucket.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    // remove_dir only removes empty dirs
                    let _ = fs::remove_dir(bucket.path());
                }
            }
        }
    }

    let remaining = kept.len();
    let remaining_bytes: u64 = kept.iter().map(|(_, s, _)| s).sum();

    Ok(GcResult { pruned, pruned_bytes, remaining, remaining_bytes })
}

pub fn clean(project_dir: &Path) -> Result<bool> {
    let dir = cache_dir(project_dir);
    if !dir.exists() {
        return Ok(false);
    }
    fs::remove_dir_all(&dir).with_context(|| format!("Failed to remove {}", dir.display()))?;
    Ok(true)
}

pub fn load_entry(project_dir: &Path, key: &str) -> Result<Option<CacheEntry>> {
    let path = cache_entry_path(project_dir, key);
    if !path.exists() {
        return Ok(None);
    }
    let content =
        fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.display()))?;
    let entry =
        serde_json::from_str(&content).with_context(|| format!("Corrupted cache entry at {}. Clear with: irradiate cache clean", path.display()))?;
    Ok(Some(entry))
}

pub fn store_entry(
    project_dir: &Path,
    key: &str,
    exit_code: i32,
    duration: f64,
    status: MutantStatus,
) -> Result<()> {
    let path = cache_entry_path(project_dir, key);
    if path.exists() {
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    let entry = CacheEntry {
        schema_version: CACHE_SCHEMA_VERSION,
        exit_code,
        duration,
        status,
    };
    let data = serde_json::to_vec_pretty(&entry)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp_path = path.with_extension(format!("tmp-{}-{nanos}", std::process::id()));
    fs::write(&tmp_path, data)
        .with_context(|| format!("Failed to write {}", tmp_path.display()))?;

    if path.exists() {
        let _ = fs::remove_file(&tmp_path);
        return Ok(());
    }

    fs::rename(&tmp_path, &path).with_context(|| {
        format!(
            "Failed to move cache entry into place: {} -> {}",
            tmp_path.display(),
            path.display()
        )
    })?;
    Ok(())
}

/// Force-overwrite a cache entry regardless of whether it already exists.
///
/// Used by the verification phase when a warm-session survivor is confirmed killed
/// in isolate mode — the stale Survived entry must be replaced with the correct
/// Killed result so that subsequent runs (which may hit the cache) get the right answer.
pub fn force_update_entry(
    project_dir: &Path,
    key: &str,
    exit_code: i32,
    duration: f64,
    status: MutantStatus,
) -> Result<()> {
    let path = cache_entry_path(project_dir, key);

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    let entry = CacheEntry {
        schema_version: CACHE_SCHEMA_VERSION,
        exit_code,
        duration,
        status,
    };
    let data = serde_json::to_vec_pretty(&entry)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp_path = path.with_extension(format!("tmp-force-{}-{nanos}", std::process::id()));
    fs::write(&tmp_path, data)
        .with_context(|| format!("Failed to write {}", tmp_path.display()))?;

    fs::rename(&tmp_path, &path).with_context(|| {
        format!(
            "Failed to move cache entry into place: {} -> {}",
            tmp_path.display(),
            path.display()
        )
    })?;
    Ok(())
}

pub fn build_cache_key(
    project_dir: &Path,
    descriptor: &MutantCacheDescriptor,
    test_ids: &[String],
    resolved_test_paths: &mut HashMap<String, Option<PathBuf>>,
    test_file_hashes: &mut HashMap<PathBuf, String>,
) -> Result<Option<String>> {
    build_cache_key_with_version(
        CACHE_VERSION,
        project_dir,
        descriptor,
        test_ids,
        resolved_test_paths,
        test_file_hashes,
    )
}

fn build_cache_key_with_version(
    version: &str,
    project_dir: &Path,
    descriptor: &MutantCacheDescriptor,
    test_ids: &[String],
    resolved_test_paths: &mut HashMap<String, Option<PathBuf>>,
    test_file_hashes: &mut HashMap<PathBuf, String>,
) -> Result<Option<String>> {
    let test_files = match resolve_test_files(project_dir, test_ids, resolved_test_paths) {
        Some(files) => files,
        None => return Ok(None),
    };

    let mut sorted_test_ids = test_ids.to_vec();
    sorted_test_ids.sort();
    let test_set_hash = hash_string_list(&sorted_test_ids);

    let mut file_entries = Vec::new();
    for path in test_files {
        let rel = path
            .strip_prefix(project_dir)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let digest = if let Some(existing) = test_file_hashes.get(&path) {
            existing.clone()
        } else {
            let bytes = fs::read(&path)
                .with_context(|| format!("Failed to read selected test file {}", path.display()))?;
            let digest = hash_bytes(&bytes);
            test_file_hashes.insert(path.clone(), digest.clone());
            digest
        };
        file_entries.push((rel, digest));
    }
    file_entries.sort();
    let test_content_hash = hash_pairs(&file_entries);

    let mut hasher = Sha256::new();
    update_field(&mut hasher, version.as_bytes());
    update_field(&mut hasher, CACHE_SCHEMA_VERSION.to_string().as_bytes());
    update_field(&mut hasher, descriptor.function_source.as_bytes());
    update_field(&mut hasher, descriptor.operator.as_bytes());
    update_field(&mut hasher, descriptor.start.to_string().as_bytes());
    update_field(&mut hasher, descriptor.end.to_string().as_bytes());
    update_field(&mut hasher, descriptor.original.as_bytes());
    update_field(&mut hasher, descriptor.replacement.as_bytes());
    update_field(&mut hasher, test_set_hash.as_bytes());
    update_field(&mut hasher, test_content_hash.as_bytes());
    Ok(Some(format!("{:x}", hasher.finalize())))
}

fn resolve_test_files(
    project_dir: &Path,
    test_ids: &[String],
    resolved_test_paths: &mut HashMap<String, Option<PathBuf>>,
) -> Option<Vec<PathBuf>> {
    let mut unique = BTreeSet::new();
    for test_id in test_ids {
        let resolved = if let Some(existing) = resolved_test_paths.get(test_id) {
            existing.clone()
        } else {
            let path_part = test_id.split("::").next().unwrap_or(test_id);
            let candidate = project_dir.join(path_part);
            let resolved = if candidate.is_file() {
                Some(candidate)
            } else {
                None
            };
            resolved_test_paths.insert(test_id.clone(), resolved.clone());
            resolved
        };
        let path = resolved?;
        unique.insert(path);
    }
    Some(unique.into_iter().collect())
}

fn cache_entry_path(project_dir: &Path, key: &str) -> PathBuf {
    let (prefix, rest) = key.split_at(2);
    cache_dir(project_dir)
        .join(prefix)
        .join(format!("{rest}.json"))
}

fn hash_string_list(items: &[String]) -> String {
    let mut hasher = Sha256::new();
    for item in items {
        update_field(&mut hasher, item.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn hash_pairs(items: &[(String, String)]) -> String {
    let mut hasher = Sha256::new();
    for (left, right) in items {
        update_field(&mut hasher, left.as_bytes());
        update_field(&mut hasher, right.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn update_field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor() -> MutantCacheDescriptor {
        MutantCacheDescriptor {
            mutant_name: "pkg.x_foo__irradiate_1".to_string(),
            function_source: "def foo(x):\n    return x + 1\n".to_string(),
            operator: "binop_swap".to_string(),
            start: 25,
            end: 26,
            original: "+".to_string(),
            replacement: "-".to_string(),
            source_file: "src/pkg/foo.py".to_string(),
            fn_byte_offset: 0,
            fn_start_line: 1,
        }
    }

    #[test]
    fn test_cache_key_stable_for_same_inputs() {
        let tmp = tempfile::tempdir().unwrap();
        let test_file = tmp.path().join("tests").join("test_mod.py");
        fs::create_dir_all(test_file.parent().unwrap()).unwrap();
        fs::write(&test_file, "def test_foo():\n    assert True\n").unwrap();
        let test_ids = vec!["tests/test_mod.py::test_foo".to_string()];

        let key_a = build_cache_key_with_version(
            "1.0.0",
            tmp.path(),
            &descriptor(),
            &test_ids,
            &mut HashMap::new(),
            &mut HashMap::new(),
        )
        .unwrap();
        let key_b = build_cache_key_with_version(
            "1.0.0",
            tmp.path(),
            &descriptor(),
            &test_ids,
            &mut HashMap::new(),
            &mut HashMap::new(),
        )
        .unwrap();

        assert_eq!(key_a, key_b);
    }

    #[test]
    fn test_cache_key_changes_when_function_source_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let test_file = tmp.path().join("tests").join("test_mod.py");
        fs::create_dir_all(test_file.parent().unwrap()).unwrap();
        fs::write(&test_file, "def test_foo():\n    assert True\n").unwrap();
        let test_ids = vec!["tests/test_mod.py::test_foo".to_string()];

        let mut desc_b = descriptor();
        desc_b.function_source = "def foo(x):\n    return x + 2\n".to_string();

        let key_a = build_cache_key_with_version(
            "1.0.0",
            tmp.path(),
            &descriptor(),
            &test_ids,
            &mut HashMap::new(),
            &mut HashMap::new(),
        )
        .unwrap()
        .unwrap();
        let key_b = build_cache_key_with_version(
            "1.0.0",
            tmp.path(),
            &desc_b,
            &test_ids,
            &mut HashMap::new(),
            &mut HashMap::new(),
        )
        .unwrap()
        .unwrap();

        assert_ne!(key_a, key_b);
    }

    #[test]
    fn test_cache_key_changes_when_test_file_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let test_file = tmp.path().join("tests").join("test_mod.py");
        fs::create_dir_all(test_file.parent().unwrap()).unwrap();
        let test_ids = vec!["tests/test_mod.py::test_foo".to_string()];

        fs::write(&test_file, "def test_foo():\n    assert True\n").unwrap();
        let key_a = build_cache_key_with_version(
            "1.0.0",
            tmp.path(),
            &descriptor(),
            &test_ids,
            &mut HashMap::new(),
            &mut HashMap::new(),
        )
        .unwrap()
        .unwrap();

        fs::write(&test_file, "def test_foo():\n    assert False\n").unwrap();
        let key_b = build_cache_key_with_version(
            "1.0.0",
            tmp.path(),
            &descriptor(),
            &test_ids,
            &mut HashMap::new(),
            &mut HashMap::new(),
        )
        .unwrap()
        .unwrap();

        assert_ne!(key_a, key_b);
    }

    #[test]
    fn test_cache_key_changes_when_test_ids_change() {
        let tmp = tempfile::tempdir().unwrap();
        let tests_dir = tmp.path().join("tests");
        fs::create_dir_all(&tests_dir).unwrap();
        fs::write(
            tests_dir.join("test_mod.py"),
            "def test_foo():\n    assert True\n",
        )
        .unwrap();
        let ids_a = vec!["tests/test_mod.py::test_foo".to_string()];
        let ids_b = vec![
            "tests/test_mod.py::test_foo".to_string(),
            "tests/test_mod.py::test_bar".to_string(),
        ];

        let key_a = build_cache_key_with_version(
            "1.0.0",
            tmp.path(),
            &descriptor(),
            &ids_a,
            &mut HashMap::new(),
            &mut HashMap::new(),
        )
        .unwrap()
        .unwrap();
        let key_b = build_cache_key_with_version(
            "1.0.0",
            tmp.path(),
            &descriptor(),
            &ids_b,
            &mut HashMap::new(),
            &mut HashMap::new(),
        )
        .unwrap()
        .unwrap();

        assert_ne!(key_a, key_b);
    }

    #[test]
    fn test_cache_key_changes_when_version_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let test_file = tmp.path().join("tests").join("test_mod.py");
        fs::create_dir_all(test_file.parent().unwrap()).unwrap();
        fs::write(&test_file, "def test_foo():\n    assert True\n").unwrap();
        let test_ids = vec!["tests/test_mod.py::test_foo".to_string()];

        let key_a = build_cache_key_with_version(
            "1.0.0",
            tmp.path(),
            &descriptor(),
            &test_ids,
            &mut HashMap::new(),
            &mut HashMap::new(),
        )
        .unwrap()
        .unwrap();
        let key_b = build_cache_key_with_version(
            "2.0.0",
            tmp.path(),
            &descriptor(),
            &test_ids,
            &mut HashMap::new(),
            &mut HashMap::new(),
        )
        .unwrap()
        .unwrap();

        assert_ne!(key_a, key_b);
    }

    #[test]
    fn test_build_cache_key_returns_none_for_unresolvable_test_id() {
        let tmp = tempfile::tempdir().unwrap();
        let test_ids = vec!["tests/missing.py::test_foo".to_string()];

        let key = build_cache_key_with_version(
            "1.0.0",
            tmp.path(),
            &descriptor(),
            &test_ids,
            &mut HashMap::new(),
            &mut HashMap::new(),
        )
        .unwrap();

        assert!(key.is_none());
    }

    #[test]
    fn test_store_and_load_entry_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        store_entry(tmp.path(), "abcdef", 1, 0.42, MutantStatus::Killed).unwrap();

        let loaded = load_entry(tmp.path(), "abcdef").unwrap().unwrap();
        assert_eq!(
            loaded,
            CacheEntry {
                schema_version: CACHE_SCHEMA_VERSION,
                exit_code: 1,
                duration: 0.42,
                status: MutantStatus::Killed,
            }
        );
    }

    #[test]
    fn test_store_entry_is_immutable() {
        let tmp = tempfile::tempdir().unwrap();
        store_entry(tmp.path(), "abcdef", 1, 0.42, MutantStatus::Killed).unwrap();
        store_entry(tmp.path(), "abcdef", 0, 9.99, MutantStatus::Survived).unwrap();

        let loaded = load_entry(tmp.path(), "abcdef").unwrap().unwrap();
        assert_eq!(loaded.exit_code, 1);
        assert_eq!(loaded.duration, 0.42);
        assert_eq!(loaded.status, MutantStatus::Killed);
    }

    #[test]
    fn test_force_update_entry_overwrites_existing() {
        // INV-4: force_update_entry must overwrite even if entry exists.
        // This is used by verification to correct false negatives.
        let tmp = tempfile::tempdir().unwrap();
        store_entry(tmp.path(), "abcdef", 0, 1.0, MutantStatus::Survived).unwrap();

        // Verify initial state
        let before = load_entry(tmp.path(), "abcdef").unwrap().unwrap();
        assert_eq!(before.status, MutantStatus::Survived);

        // Force-update to Killed
        force_update_entry(tmp.path(), "abcdef", 1, 0.5, MutantStatus::Killed).unwrap();

        let after = load_entry(tmp.path(), "abcdef").unwrap().unwrap();
        assert_eq!(after.status, MutantStatus::Killed);
        assert_eq!(after.exit_code, 1);
    }

    #[test]
    fn test_force_update_entry_creates_if_absent() {
        // force_update_entry must also work when no entry exists yet.
        let tmp = tempfile::tempdir().unwrap();
        force_update_entry(tmp.path(), "newkey", 1, 0.3, MutantStatus::Killed).unwrap();

        let loaded = load_entry(tmp.path(), "newkey").unwrap().unwrap();
        assert_eq!(loaded.status, MutantStatus::Killed);
        assert_eq!(loaded.exit_code, 1);
    }

    #[test]
    fn test_clean_removes_only_cache_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let irr_dir = tmp.path().join(".irradiate");
        let cache = irr_dir.join("cache");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("entry"), "x").unwrap();
        fs::write(irr_dir.join("stats.json"), "{}").unwrap();

        let removed = clean(tmp.path()).unwrap();

        assert!(removed);
        assert!(!cache.exists());
        assert!(irr_dir.join("stats.json").exists());
    }

    // --- Duration parsing tests ---

    #[test]
    fn test_parse_duration_days() {
        assert_eq!(parse_duration("30d").unwrap(), 30 * 86400);
    }

    #[test]
    fn test_parse_duration_hours() {
        assert_eq!(parse_duration("24h").unwrap(), 24 * 3600);
    }

    #[test]
    fn test_parse_duration_combined() {
        assert_eq!(parse_duration("1h30m").unwrap(), 3600 + 30 * 60);
    }

    #[test]
    fn test_parse_duration_complex() {
        assert_eq!(parse_duration("1d12h").unwrap(), 86400 + 12 * 3600);
    }

    #[test]
    fn test_parse_duration_seconds() {
        assert_eq!(parse_duration("3600s").unwrap(), 3600);
    }

    #[test]
    fn test_parse_duration_invalid_no_unit() {
        assert!(parse_duration("30").is_err());
    }

    #[test]
    fn test_parse_duration_invalid_unit() {
        assert!(parse_duration("30x").is_err());
    }

    #[test]
    fn test_parse_duration_empty() {
        assert!(parse_duration("").is_err());
    }

    // --- Size parsing tests ---

    #[test]
    fn test_parse_size_megabytes() {
        assert_eq!(parse_size("500mb").unwrap(), 500 * 1024 * 1024);
    }

    #[test]
    fn test_parse_size_gigabytes() {
        assert_eq!(parse_size("1gb").unwrap(), 1024 * 1024 * 1024);
    }

    #[test]
    fn test_parse_size_kilobytes() {
        assert_eq!(parse_size("100kb").unwrap(), 100 * 1024);
    }

    #[test]
    fn test_parse_size_case_insensitive() {
        assert_eq!(parse_size("1GB").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("500MB").unwrap(), 500 * 1024 * 1024);
    }

    #[test]
    fn test_parse_size_invalid() {
        assert!(parse_size("").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("100xx").is_err());
    }

    // --- GC tests ---

    /// Helper: create a fake cache entry file in the proper bucket structure.
    fn create_cache_file(project_dir: &Path, key: &str, content: &str) -> PathBuf {
        let (prefix, rest) = key.split_at(2);
        let dir = cache_dir(project_dir).join(prefix);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{rest}.json"));
        fs::write(&path, content).unwrap();
        path
    }

    /// Helper: backdate a file's mtime by the given number of seconds.
    fn backdate_mtime(path: &Path, age_secs: u64) {
        use std::time::Duration;
        let mtime = SystemTime::now() - Duration::from_secs(age_secs);
        let ft = filetime::FileTime::from_system_time(mtime);
        filetime::set_file_mtime(path, ft).unwrap();
    }

    #[test]
    fn test_gc_empty_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let result = gc(tmp.path(), 86400 * 30, 1024 * 1024 * 1024, false).unwrap();
        assert_eq!(result.pruned, 0);
        assert_eq!(result.remaining, 0);
    }

    #[test]
    fn test_gc_max_age_prunes_old_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let old = create_cache_file(tmp.path(), "aabbcc", r#"{"schema_version":1}"#);
        let _new = create_cache_file(tmp.path(), "ddee ff", r#"{"schema_version":1}"#);
        // Make old entry 10 days old
        backdate_mtime(&old, 10 * 86400);

        let result = gc(tmp.path(), 7 * 86400, u64::MAX, false).unwrap();
        assert_eq!(result.pruned, 1);
        assert_eq!(result.remaining, 1);
        assert!(!old.exists());
    }

    #[test]
    fn test_gc_max_size_evicts_oldest() {
        let tmp = tempfile::tempdir().unwrap();
        let content = "x".repeat(1000);
        let p1 = create_cache_file(tmp.path(), "aabbcc", &content);
        let p2 = create_cache_file(tmp.path(), "ddeeff", &content);
        let p3 = create_cache_file(tmp.path(), "gghhii", &content);
        // Make p1 oldest, p3 newest
        backdate_mtime(&p1, 300);
        backdate_mtime(&p2, 200);
        // p3 is current

        // Max size = 2500 bytes (fits 2 of 3 entries at 1000 each)
        let result = gc(tmp.path(), u64::MAX, 2500, false).unwrap();
        assert_eq!(result.pruned, 1);
        assert_eq!(result.remaining, 2);
        // Oldest should be evicted
        assert!(!p1.exists());
        assert!(p2.exists());
        assert!(p3.exists());
    }

    #[test]
    fn test_gc_dry_run_does_not_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let old = create_cache_file(tmp.path(), "aabbcc", "data");
        backdate_mtime(&old, 100 * 86400);

        let result = gc(tmp.path(), 1 * 86400, u64::MAX, true).unwrap();
        assert_eq!(result.pruned, 1);
        // File still exists
        assert!(old.exists());
    }

    #[test]
    fn test_gc_max_age_zero_prunes_all() {
        let tmp = tempfile::tempdir().unwrap();
        let p1 = create_cache_file(tmp.path(), "aabbcc", "data1");
        let p2 = create_cache_file(tmp.path(), "ddeeff", "data2");
        // Backdate both by 2 seconds to ensure they're older than max_age=0
        backdate_mtime(&p1, 2);
        backdate_mtime(&p2, 2);

        let result = gc(tmp.path(), 0, u64::MAX, false).unwrap();
        assert_eq!(result.pruned, 2);
        assert_eq!(result.remaining, 0);
    }
}
