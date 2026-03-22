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

pub fn cache_dir(project_dir: &Path) -> PathBuf {
    project_dir.join(".irradiate").join("cache")
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
        serde_json::from_str(&content).with_context(|| format!("Invalid {}", path.display()))?;
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
}
