use irradiate::codegen;
use irradiate::harness;
use irradiate::orchestrator::{run_worker_pool, PoolConfig};
use irradiate::pipeline;
use irradiate::protocol::{MutantStatus, WorkItem};
use irradiate::stats;
use std::fs;
use std::path::{Path, PathBuf};

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Find the Python interpreter in the test fixture's venv.
fn fixture_python() -> PathBuf {
    let root = project_root();
    let venv_python = root.join("tests/fixtures/simple_project/.venv/bin/python3");
    if venv_python.exists() {
        venv_python
    } else {
        PathBuf::from("python3")
    }
}

fn fixture_dir() -> PathBuf {
    project_root().join("tests/fixtures/simple_project")
}

/// Generate mutants on the fly from the fixture source, writing to a temp directory.
/// Returns (TempDir handle, list of mutant keys).
fn generate_test_mutants() -> (tempfile::TempDir, Vec<String>) {
    let source = fs::read_to_string(fixture_dir().join("src/simple_lib/__init__.py")).unwrap();
    let mutated =
        codegen::mutate_file(&source, "simple_lib", None).expect("fixture should produce mutations");

    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("simple_lib")).unwrap();
    fs::write(tmp.path().join("simple_lib/__init__.py"), &mutated.source).unwrap();

    (tmp, mutated.mutant_names)
}

fn generate_mutants_for_project(
    project_dir: &Path,
    source_rel_path: &str,
    module_name: &str,
) -> (tempfile::TempDir, Vec<String>) {
    let source = fs::read_to_string(project_dir.join(source_rel_path)).unwrap();
    let mutated =
        codegen::mutate_file(&source, module_name, None).expect("fixture should produce mutations");

    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join(module_name)).unwrap();
    fs::write(
        tmp.path().join(module_name).join("__init__.py"),
        &mutated.source,
    )
    .unwrap();

    (tmp, mutated.mutant_names)
}

#[tokio::test]
async fn test_worker_pool_dispatches_mutants() {
    let fixture = fixture_dir();
    let python = fixture_python();
    let (_tmp, mutant_names) = generate_test_mutants();

    assert!(
        fixture.join("tests/test_simple.py").exists(),
        "Test fixture must exist"
    );

    let config = PoolConfig {
        num_workers: 2,
        python,
        project_dir: fixture.clone(),
        mutants_dir: _tmp.path().to_path_buf(),
        tests_dir: fixture.join("tests"),
        timeout_multiplier: 10.0,
        ..Default::default()
    };

    // Pick one add mutant (should be killed by test_add)
    let add_mutant = mutant_names
        .iter()
        .find(|n| n.contains("x_add__irradiate_"))
        .expect("Should have an add mutant");

    // Pick an is_positive mutant (should be killed by test_is_positive)
    let is_pos_mutant = mutant_names
        .iter()
        .find(|n| n.contains("x_is_positive__irradiate_"))
        .expect("Should have an is_positive mutant");

    // Pick a greet mutant (should be killed by test_greet)
    let greet_mutant = mutant_names
        .iter()
        .find(|n| n.contains("x_greet__irradiate_"))
        .expect("Should have a greet mutant");

    let work_items = vec![
        WorkItem {
            mutant_name: add_mutant.clone(),
            test_ids: vec!["tests/test_simple.py::test_add".to_string()],
            estimated_duration_secs: 0.0,
            timeout_secs: 300.0,
        },
        WorkItem {
            mutant_name: is_pos_mutant.clone(),
            test_ids: vec!["tests/test_simple.py::test_is_positive".to_string()],
            estimated_duration_secs: 0.0,
            timeout_secs: 300.0,
        },
        WorkItem {
            mutant_name: greet_mutant.clone(),
            test_ids: vec!["tests/test_simple.py::test_greet".to_string()],
            estimated_duration_secs: 0.0,
            timeout_secs: 300.0,
        },
    ];

    let (results, _trace) = run_worker_pool(&config, work_items, None)
        .await
        .expect("Worker pool should complete");

    assert_eq!(results.len(), 3, "Should have results for all 3 mutants");

    for result in &results {
        println!(
            "  {} -> {:?} (exit_code={}, duration={:.3}s)",
            result.mutant_name, result.status, result.exit_code, result.duration
        );
        assert_eq!(
            result.status,
            MutantStatus::Killed,
            "Mutant {} should be killed, got {:?}",
            result.mutant_name,
            result.status
        );
    }
}

#[tokio::test]
async fn test_worker_pool_empty_work() {
    let fixture = fixture_dir();
    let python = fixture_python();
    let (_tmp, _) = generate_test_mutants();

    let config = PoolConfig {
        num_workers: 1,
        python,
        project_dir: fixture.clone(),
        mutants_dir: _tmp.path().to_path_buf(),
        tests_dir: fixture.join("tests"),
        ..Default::default()
    };

    let (results, _trace) = run_worker_pool(&config, vec![], None)
        .await
        .expect("Empty work should succeed");
    assert!(results.is_empty());
}

#[tokio::test]
async fn test_worker_pool_surviving_mutant() {
    let fixture = fixture_dir();
    let python = fixture_python();
    let (_tmp, _) = generate_test_mutants();

    let config = PoolConfig {
        num_workers: 1,
        python,
        project_dir: fixture.clone(),
        mutants_dir: _tmp.path().to_path_buf(),
        tests_dir: fixture.join("tests"),
        ..Default::default()
    };

    // Use a mutant name that doesn't match any function — the trampoline
    // will call the original, so tests pass -> survived.
    let work_items = vec![WorkItem {
        mutant_name: "simple_lib.x_nonexistent__irradiate_1".to_string(),
        test_ids: vec![
            "tests/test_simple.py::test_add".to_string(),
            "tests/test_simple.py::test_is_positive".to_string(),
            "tests/test_simple.py::test_greet".to_string(),
        ],
        estimated_duration_secs: 0.0,
        timeout_secs: 300.0,
    }];

    let (results, _trace) = run_worker_pool(&config, work_items, None)
        .await
        .expect("Worker pool should complete");
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].status,
        MutantStatus::Survived,
        "Non-matching mutant should survive (trampoline falls through to original)"
    );
}

/// INV-1: With recycling enabled, results must be identical to no-recycling.
/// Use recycle_after=1 to force recycling after every single mutant — maximally exercises
/// the recycle code path.
#[tokio::test]
async fn test_worker_pool_with_recycling() {
    let fixture = fixture_dir();
    let python = fixture_python();
    let (_tmp, mutant_names) = generate_test_mutants();

    assert!(
        fixture.join("tests/test_simple.py").exists(),
        "Test fixture must exist"
    );

    let add_mutant = mutant_names
        .iter()
        .find(|n| n.contains("x_add__irradiate_"))
        .expect("Should have an add mutant");

    let is_pos_mutant = mutant_names
        .iter()
        .find(|n| n.contains("x_is_positive__irradiate_"))
        .expect("Should have an is_positive mutant");

    let greet_mutant = mutant_names
        .iter()
        .find(|n| n.contains("x_greet__irradiate_"))
        .expect("Should have a greet mutant");

    let work_items = vec![
        WorkItem {
            mutant_name: add_mutant.clone(),
            test_ids: vec!["tests/test_simple.py::test_add".to_string()],
            estimated_duration_secs: 0.0,
            timeout_secs: 300.0,
        },
        WorkItem {
            mutant_name: is_pos_mutant.clone(),
            test_ids: vec!["tests/test_simple.py::test_is_positive".to_string()],
            estimated_duration_secs: 0.0,
            timeout_secs: 300.0,
        },
        WorkItem {
            mutant_name: greet_mutant.clone(),
            test_ids: vec!["tests/test_simple.py::test_greet".to_string()],
            estimated_duration_secs: 0.0,
            timeout_secs: 300.0,
        },
    ];

    // recycle_after=Some(1): worker is replaced after every single mutant (explicit)
    let config = PoolConfig {
        num_workers: 1,
        python,
        project_dir: fixture.clone(),
        mutants_dir: _tmp.path().to_path_buf(),
        tests_dir: fixture.join("tests"),
        timeout_multiplier: 10.0,
        worker_recycle_after: Some(1),
        ..Default::default()
    };

    let (results, _trace) = run_worker_pool(&config, work_items, None)
        .await
        .expect("Worker pool should complete with recycling");

    assert_eq!(results.len(), 3, "Should have results for all 3 mutants");

    for result in &results {
        println!(
            "  {} -> {:?} (exit_code={}, duration={:.3}s)",
            result.mutant_name, result.status, result.exit_code, result.duration
        );
        assert_eq!(
            result.status,
            MutantStatus::Killed,
            "Mutant {} should be killed even with recycling, got {:?}",
            result.mutant_name,
            result.status
        );
    }
}

/// Verify that recycle_after=0 disables recycling (single long-lived worker per slot).
#[tokio::test]
async fn test_worker_pool_recycle_disabled() {
    let fixture = fixture_dir();
    let python = fixture_python();
    let (_tmp, mutant_names) = generate_test_mutants();

    let add_mutant = mutant_names
        .iter()
        .find(|n| n.contains("x_add__irradiate_"))
        .expect("Should have an add mutant");

    let work_items = vec![WorkItem {
        mutant_name: add_mutant.clone(),
        test_ids: vec!["tests/test_simple.py::test_add".to_string()],
        estimated_duration_secs: 0.0,
        timeout_secs: 300.0,
    }];

    let config = PoolConfig {
        num_workers: 1,
        python,
        project_dir: fixture.clone(),
        mutants_dir: _tmp.path().to_path_buf(),
        tests_dir: fixture.join("tests"),
        timeout_multiplier: 10.0,
        worker_recycle_after: Some(0), // disabled
        ..Default::default()
    };

    let (results, _trace) = run_worker_pool(&config, work_items, None)
        .await
        .expect("Worker pool should complete with recycling disabled");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, MutantStatus::Killed);
}

#[tokio::test]
async fn test_worker_pool_repeated_runs_same_worker_cleanup() {
    let fixture = fixture_dir();
    let python = fixture_python();
    let (_tmp, mutant_names) = generate_test_mutants();

    let add_mutant = mutant_names
        .iter()
        .find(|n| n.contains("x_add__irradiate_"))
        .expect("Should have an add mutant");

    let config = PoolConfig {
        num_workers: 1,
        python,
        project_dir: fixture.clone(),
        mutants_dir: _tmp.path().to_path_buf(),
        tests_dir: fixture.join("tests"),
        timeout_multiplier: 10.0,
        worker_recycle_after: Some(0), // disabled
        ..Default::default()
    };

    let work_items = vec![
        WorkItem {
            mutant_name: add_mutant.clone(),
            test_ids: vec!["tests/test_simple.py::test_add".to_string()],
            estimated_duration_secs: 0.0,
            timeout_secs: 300.0,
        },
        WorkItem {
            mutant_name: "simple_lib.x_nonexistent__irradiate_1".to_string(),
            test_ids: vec![
                "tests/test_simple.py::test_add".to_string(),
                "tests/test_simple.py::test_is_positive".to_string(),
                "tests/test_simple.py::test_greet".to_string(),
            ],
            estimated_duration_secs: 0.0,
            timeout_secs: 300.0,
        },
    ];

    let (results, _trace) = run_worker_pool(&config, work_items, None)
        .await
        .expect("Worker pool should complete with repeated runs on one worker");

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].status, MutantStatus::Killed);
    assert_eq!(
        results[1].status,
        MutantStatus::Survived,
        "Second run on the same worker must not inherit the first run's failure state"
    );
}

#[tokio::test]
async fn test_worker_pool_repeated_runs_teardown_module_fixture() {
    let project = tempfile::tempdir().unwrap();
    fs::create_dir_all(project.path().join("src/stateful")).unwrap();
    fs::create_dir_all(project.path().join("tests")).unwrap();

    fs::write(
        project.path().join("src/stateful/__init__.py"),
        r#"STATE = []

def mark():
    STATE.append("x")
    return len(STATE) == 1
"#,
    )
    .unwrap();
    fs::write(
        project.path().join("tests/test_stateful.py"),
        r#"import pytest

from stateful import STATE, mark


@pytest.fixture(scope="module", autouse=True)
def reset_state():
    STATE.clear()
    yield
    STATE.clear()


def test_mark_once():
    assert mark() is True
"#,
    )
    .unwrap();

    let python = fixture_python();
    let (_tmp, _) =
        generate_mutants_for_project(project.path(), "src/stateful/__init__.py", "stateful");
    let harness_dir = harness::extract_harness(project.path()).expect("harness extraction");
    let pythonpath = pipeline::build_pythonpath(&harness_dir, &[project.path().join("src")]);
    let mutants_dir = _tmp.path().to_path_buf();
    let test_stats =
        stats::collect_stats(&python, project.path(), &pythonpath, &mutants_dir, "tests", &[])
            .expect("Temp project stats collection should succeed");
    let test_id = test_stats
        .duration_by_test
        .keys()
        .next()
        .cloned()
        .expect("Temp project should collect one test");
    let mut candidate_test_ids = vec![test_id.clone()];
    if let Some(stripped) = test_id.strip_prefix("tests/") {
        candidate_test_ids.push(stripped.to_string());
    } else {
        candidate_test_ids.push(format!("tests/{test_id}"));
    }
    let config = PoolConfig {
        num_workers: 1,
        python,
        project_dir: project.path().to_path_buf(),
        mutants_dir,
        tests_dir: project.path().join("tests"),
        timeout_multiplier: 10.0,
        pythonpath,
        worker_recycle_after: Some(0), // disabled
        ..Default::default()
    };

    let work_items = vec![
        WorkItem {
            mutant_name: "stateful.x_nonexistent__irradiate_1".to_string(),
            test_ids: candidate_test_ids.clone(),
            estimated_duration_secs: 0.0,
            timeout_secs: 300.0,
        },
        WorkItem {
            mutant_name: "stateful.x_nonexistent__irradiate_1".to_string(),
            test_ids: candidate_test_ids,
            estimated_duration_secs: 0.0,
            timeout_secs: 300.0,
        },
    ];

    let (results, _trace) = run_worker_pool(&config, work_items, None)
        .await
        .expect("Worker pool should cleanly rerun module-scoped fixtures");

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].status, MutantStatus::Survived);
    assert_eq!(
        results[1].status,
        MutantStatus::Survived,
        "Module-scoped fixture teardown must reset state between runs on one worker"
    );
}

/// INV-1: Module global modified by test in mutant run N is restored before run N+1.
///
/// The test sets `leaky.SETTING = True` and asserts it starts as `False`.
/// Without module restore, run 2 sees `True` from run 1 → assertion fails → killed (wrong).
/// With module restore, run 2 sees the original `False` → assertion passes → survived (correct).
#[tokio::test]
async fn test_module_global_restore_between_runs() {
    let project = tempfile::tempdir().unwrap();
    fs::create_dir_all(project.path().join("src/leaky")).unwrap();
    fs::create_dir_all(project.path().join("tests")).unwrap();

    // Include a binop so mutate_file produces at least one mutation.
    // The test only touches SETTING (module-level global), not the add function.
    fs::write(
        project.path().join("src/leaky/__init__.py"),
        "SETTING = False\n\ndef add(a, b):\n    return a + b\n",
    )
    .unwrap();

    // This test asserts SETTING starts False, then sets it to True.
    // If module state leaks across runs, run 2 sees True from the start → assertion fails.
    fs::write(
        project.path().join("tests/test_leaky.py"),
        concat!(
            "import leaky\n",
            "\n",
            "def test_setting_starts_false():\n",
            "    assert leaky.SETTING is False, ",
            r#"f"SETTING should be False at start of run, got {leaky.SETTING}""#,
            "\n",
            "    leaky.SETTING = True\n",
        ),
    )
    .unwrap();

    let python = fixture_python();
    let (_tmp, _) =
        generate_mutants_for_project(project.path(), "src/leaky/__init__.py", "leaky");
    let harness_dir = harness::extract_harness(project.path()).expect("harness extraction");
    let pythonpath = pipeline::build_pythonpath(&harness_dir, &[project.path().join("src")]);
    let mutants_dir = _tmp.path().to_path_buf();

    // Use stats collection to discover the actual test ID format (pytest nodeids vary).
    let test_stats =
        stats::collect_stats(&python, project.path(), &pythonpath, &mutants_dir, "tests", &[])
            .expect("leaky project stats collection should succeed");
    let test_id = test_stats
        .duration_by_test
        .keys()
        .next()
        .cloned()
        .expect("leaky project should have one test");
    let mut candidate_test_ids = vec![test_id.clone()];
    if let Some(stripped) = test_id.strip_prefix("tests/") {
        candidate_test_ids.push(stripped.to_string());
    } else {
        candidate_test_ids.push(format!("tests/{test_id}"));
    }

    let config = PoolConfig {
        num_workers: 1,
        python,
        project_dir: project.path().to_path_buf(),
        mutants_dir,
        tests_dir: project.path().join("tests"),
        timeout_multiplier: 10.0,
        pythonpath,
        worker_recycle_after: Some(0), // No recycling — we test module restore, not recycling
        ..Default::default()
    };

    // Use nonexistent mutant names: trampoline calls original, so tests see real behavior.
    // Both runs use the same test which asserts SETTING starts False.
    let work_items = vec![
        WorkItem {
            mutant_name: "leaky.x_nonexistent__irradiate_1".to_string(),
            test_ids: candidate_test_ids.clone(),
            estimated_duration_secs: 0.0,
            timeout_secs: 300.0,
        },
        WorkItem {
            mutant_name: "leaky.x_nonexistent__irradiate_2".to_string(),
            test_ids: candidate_test_ids,
            estimated_duration_secs: 0.0,
            timeout_secs: 300.0,
        },
    ];

    let (results, _trace) = run_worker_pool(&config, work_items, None)
        .await
        .expect("Worker pool should complete");

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].status, MutantStatus::Survived, "First run should survive");
    assert_eq!(
        results[1].status,
        MutantStatus::Survived,
        "INV-1: second run must see restored SETTING=False, not leaked True from run 1"
    );
}

/// INV-2: Trampolined function dispatch still works across multiple fork-mode runs on the same worker.
///
/// In fork mode each child gets a fresh COW copy; the parent's trampoline dict is intact
/// across runs. We run the same mutant twice and verify it is killed both times.
#[tokio::test]
async fn test_trampoline_intact_after_restore() {
    let fixture = fixture_dir();
    let python = fixture_python();
    let (_tmp, mutant_names) = generate_test_mutants();

    let add_mutant = mutant_names
        .iter()
        .find(|n| n.contains("x_add__irradiate_"))
        .expect("Should have an add mutant");

    // Run two mutants on the same worker (no recycling).
    // First run: add_mutant → should be killed.
    // Second run: add_mutant again → should also be killed (trampoline intact after restore).
    let work_items = vec![
        WorkItem {
            mutant_name: add_mutant.clone(),
            test_ids: vec!["tests/test_simple.py::test_add".to_string()],
            estimated_duration_secs: 0.0,
            timeout_secs: 300.0,
        },
        WorkItem {
            mutant_name: add_mutant.clone(),
            test_ids: vec!["tests/test_simple.py::test_add".to_string()],
            estimated_duration_secs: 0.0,
            timeout_secs: 300.0,
        },
    ];

    let config = PoolConfig {
        num_workers: 1,
        python,
        project_dir: fixture.clone(),
        mutants_dir: _tmp.path().to_path_buf(),
        tests_dir: fixture.join("tests"),
        timeout_multiplier: 10.0,
        worker_recycle_after: Some(0), // same worker for both runs
        ..Default::default()
    };

    let (results, _trace) = run_worker_pool(&config, work_items, None)
        .await
        .expect("Worker pool should complete");

    assert_eq!(results.len(), 2);
    assert_eq!(
        results[0].status,
        MutantStatus::Killed,
        "INV-2: first run must kill the mutant"
    );
    assert_eq!(
        results[1].status,
        MutantStatus::Killed,
        "INV-2: trampoline must still dispatch mutant correctly on second fork"
    );
}

/// Build a minimal temp project with a session-scoped fixture.
/// Returns (TempDir, harness_dir, pythonpath, mutants_dir, mutant_names, test_id).
async fn build_session_fixture_project(
    lib_name: &str,
    fixture_name: &str,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    String,
    tempfile::TempDir,
    Vec<String>,
    String,
) {
    let project = tempfile::tempdir().unwrap();
    let lib_dir = format!("src/{lib_name}");
    fs::create_dir_all(project.path().join(&lib_dir)).unwrap();
    fs::create_dir_all(project.path().join("tests")).unwrap();

    // pyproject.toml: anchors pytest rootdir and adds src to pythonpath
    fs::write(
        project.path().join("pyproject.toml"),
        format!("[tool.pytest.ini_options]\ntestpaths = [\"tests\"]\npythonpath = [\"src\"]\n"),
    )
    .unwrap();

    fs::write(
        project.path().join(format!("src/{lib_name}/__init__.py")),
        "def add(a, b):\n    return a + b\n",
    )
    .unwrap();

    fs::write(
        project.path().join("tests/conftest.py"),
        format!(
            "import pytest\n\n@pytest.fixture(scope=\"session\")\ndef {fixture_name}():\n    return {{\"count\": 0}}\n"
        ),
    )
    .unwrap();

    let test_file = format!("tests/test_{lib_name}.py");
    fs::write(
        project.path().join(&test_file),
        format!(
            "from {lib_name} import add\n\ndef test_add({fixture_name}):\n    {fixture_name}[\"count\"] += 1\n    assert add(1, 2) == 3\n"
        ),
    )
    .unwrap();

    let (_mutants_tmp, mutant_names) = generate_mutants_for_project(
        project.path(),
        &format!("src/{lib_name}/__init__.py"),
        lib_name,
    );

    let harness_dir = harness::extract_harness(project.path()).expect("harness extraction");
    let pythonpath = pipeline::build_pythonpath(&harness_dir, &[project.path().join("src")]);
    let mutants_dir = _mutants_tmp.path().to_path_buf();

    // Collect the real test ID via stats
    let python = fixture_python();
    let test_stats =
        stats::collect_stats(&python, project.path(), &pythonpath, &mutants_dir, "tests", &[])
            .expect("Stats collection should succeed");
    let test_id = test_stats
        .duration_by_test
        .keys()
        .next()
        .cloned()
        .expect("Should collect one test");

    (
        project,
        harness_dir,
        pythonpath,
        _mutants_tmp,
        mutant_names,
        test_id,
    )
}

/// INV-1/INV-4: A project with session-scoped fixtures produces correct results under
/// the worker pool. This test verifies that the pool works end-to-end when session
/// fixtures are present; the auto-tuned recycle interval does not cause test failures.
/// (Precise recycle-interval verification lives in orchestrator unit tests.)
#[tokio::test]
async fn test_worker_pool_with_session_fixture_project() {
    let (project, _harness_dir, pythonpath, _mutants_tmp, mutant_names, test_id) =
        build_session_fixture_project("session_lib", "session_counter").await;

    let add_mutant = mutant_names
        .iter()
        .find(|n| n.contains("x_add__irradiate_"))
        .expect("Should have an add mutant");

    // Use auto-tuning (None) — the orchestrator should detect session fixtures and log
    let config = PoolConfig {
        num_workers: 1,
        python: fixture_python(),
        project_dir: project.path().to_path_buf(),
        mutants_dir: _mutants_tmp.path().to_path_buf(),
        tests_dir: project.path().join("tests"),
        timeout_multiplier: 10.0,
        pythonpath,
        worker_recycle_after: None, // auto-tune
        ..Default::default()
    };

    let work_items = vec![WorkItem {
        mutant_name: add_mutant.clone(),
        test_ids: vec![test_id],
        estimated_duration_secs: 0.0,
        timeout_secs: 300.0,
    }];

    let (results, _trace) = run_worker_pool(&config, work_items, None)
        .await
        .expect("Worker pool should complete with session fixture project");

    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].status,
        MutantStatus::Killed,
        "Session fixture project: add mutant should be killed"
    );
}

/// INV-2: Explicit --worker-recycle-after is always respected regardless of session fixtures.
/// This tests that Some(n) bypasses auto-tune logic.
#[tokio::test]
async fn test_worker_pool_explicit_recycle_overrides_auto_tune() {
    let (project, _harness_dir, pythonpath, _mutants_tmp, mutant_names, test_id) =
        build_session_fixture_project("explicit_lib", "heavy_resource").await;

    let add_mutant = mutant_names
        .iter()
        .find(|n| n.contains("x_add__irradiate_"))
        .expect("Should have an add mutant");

    // Explicit recycle after 50 — should not be reduced despite session fixtures
    let config = PoolConfig {
        num_workers: 1,
        python: fixture_python(),
        project_dir: project.path().to_path_buf(),
        mutants_dir: _mutants_tmp.path().to_path_buf(),
        tests_dir: project.path().join("tests"),
        timeout_multiplier: 10.0,
        pythonpath,
        worker_recycle_after: Some(50), // explicit — must not be auto-tuned
        ..Default::default()
    };

    let work_items = vec![WorkItem {
        mutant_name: add_mutant.clone(),
        test_ids: vec![test_id],
        estimated_duration_secs: 0.0,
        timeout_secs: 300.0,
    }];

    let (results, _trace) = run_worker_pool(&config, work_items, None)
        .await
        .expect("Worker pool should complete with explicit recycle setting");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, MutantStatus::Killed);
}

/// INV-3: A child process that calls os._exit() in fork mode produces a result (not a hang).
///
/// In fork mode, os._exit() in the child only kills the child; the parent survives and
/// reports exit code 1 → MutantStatus::Killed. The key invariant is no hang: run_worker_pool
/// must return even when a test abruptly exits the child process.
#[tokio::test]
async fn test_worker_crash_produces_error_not_hang() {
    let crash_project = project_root().join("tests/fixtures/crash_worker_project");
    let python = fixture_python();

    let (_tmp, mutant_names) = generate_mutants_for_project(
        &crash_project,
        "src/crash_target/__init__.py",
        "crash_target",
    );

    let harness_dir = harness::extract_harness(&crash_project).expect("harness extraction");
    let pythonpath = pipeline::build_pythonpath(&harness_dir, &[crash_project.join("src")]);
    let mutants_dir = _tmp.path().to_path_buf();

    // Use stats collection to discover the real test node ID (format varies by pytest/OS).
    let test_stats = stats::collect_stats(
        &python,
        &crash_project,
        &pythonpath,
        &mutants_dir,
        "tests",
        &[],
    )
    .expect("Stats collection should succeed (test passes in stats mode)");

    let test_id = test_stats
        .duration_by_test
        .keys()
        .next()
        .cloned()
        .expect("crash_worker_project should collect one test");

    // Also try alternate prefix form so _prepare_items lookup succeeds regardless of rootdir.
    let mut candidate_ids = vec![test_id.clone()];
    if let Some(stripped) = test_id.strip_prefix("tests/") {
        candidate_ids.push(stripped.to_string());
    } else {
        candidate_ids.push(format!("tests/{test_id}"));
    }

    let add_mutant = mutant_names
        .iter()
        .find(|n| n.contains("x_add__irradiate_"))
        .expect("crash_target fixture must have an add mutant");

    let config = PoolConfig {
        num_workers: 1,
        python,
        project_dir: crash_project.clone(),
        mutants_dir,
        tests_dir: crash_project.join("tests"),
        timeout_multiplier: 10.0,
        pythonpath,
        worker_recycle_after: Some(0), // no count recycling — only crash path matters
        ..Default::default()
    };

    let work_items = vec![WorkItem {
        mutant_name: add_mutant.clone(),
        test_ids: candidate_ids,
        estimated_duration_secs: 0.0,
        timeout_secs: 60.0,
    }];

    // Must not hang. If disconnect handling is broken, run_worker_pool never returns.
    let (results, _trace) = run_worker_pool(&config, work_items, None)
        .await
        .expect("Worker pool must not hang when the worker process crashes");

    assert_eq!(results.len(), 1, "Forked child crash must still produce exactly one result (no hang)");
    // In fork mode, os._exit(1) only kills the child; the parent receives exit code 1 → Killed.
    assert_eq!(
        results[0].status,
        MutantStatus::Killed,
        "os._exit(1) in the child must produce Killed status in fork mode, got {:?}",
        results[0].status
    );
}

/// Memory limit recycling: run completes correctly when max_worker_memory_mb is set
/// to a very low value (1 MB). Any Python process far exceeds 1 MB RSS, so workers
/// will be flagged for recycling by the periodic memory check.
///
/// This exercises the memory-recycle code path in dispatch_work (lines ~634-655):
/// check_rss → flag worker → memory_recycle=true on next result → spawn replacement.
/// If the memory recycle path hangs or drops work items, this test will fail.
#[tokio::test]
async fn test_memory_limit_run_completes() {
    let fixture = fixture_dir();
    let python = fixture_python();
    let (_tmp, mutant_names) = generate_test_mutants();

    let add_mutant = mutant_names
        .iter()
        .find(|n| n.contains("x_add__irradiate_"))
        .expect("Should have an add mutant");
    let is_pos_mutant = mutant_names
        .iter()
        .find(|n| n.contains("x_is_positive__irradiate_"))
        .expect("Should have an is_positive mutant");

    // 1 MB limit: any Python process will far exceed this.
    // The periodic memory check will flag each worker after it starts.
    // Setting worker_recycle_after=Some(0) disables count recycling so only memory
    // recycling can trigger respawn — exercises that specific code path.
    let config = PoolConfig {
        num_workers: 1,
        python,
        project_dir: fixture.clone(),
        mutants_dir: _tmp.path().to_path_buf(),
        tests_dir: fixture.join("tests"),
        timeout_multiplier: 10.0,
        max_worker_memory_mb: 1, // any Python process exceeds this
        worker_recycle_after: Some(0), // count recycling disabled; only memory path active
        ..Default::default()
    };

    let work_items = vec![
        WorkItem {
            mutant_name: add_mutant.clone(),
            test_ids: vec!["tests/test_simple.py::test_add".to_string()],
            estimated_duration_secs: 0.0,
            timeout_secs: 300.0,
        },
        WorkItem {
            mutant_name: is_pos_mutant.clone(),
            test_ids: vec!["tests/test_simple.py::test_is_positive".to_string()],
            estimated_duration_secs: 0.0,
            timeout_secs: 300.0,
        },
    ];

    // Must complete without hanging, even if memory recycling respawns workers mid-run.
    let (results, _trace) = run_worker_pool(&config, work_items, None)
        .await
        .expect("Worker pool must complete even with aggressive memory recycling");

    assert_eq!(results.len(), 2, "All mutants must produce results despite memory recycling");
    // Memory recycling must not corrupt results: each status must be a valid classification.
    for result in &results {
        assert!(
            matches!(
                result.status,
                MutantStatus::Killed | MutantStatus::Survived | MutantStatus::Error
            ),
            "Mutant {} must have a valid status after memory recycling, got {:?}",
            result.mutant_name,
            result.status
        );
    }
}

#[tokio::test]
async fn test_stats_collection() {
    let fixture = fixture_dir();
    let python = fixture_python();
    let (_tmp, _) = generate_test_mutants();

    // Extract harness
    let harness_dir = harness::extract_harness(&fixture).expect("harness extraction");

    let mutants_dir = _tmp.path().to_path_buf();
    let pythonpath = pipeline::build_pythonpath(&harness_dir, &[fixture.join("src")]);
    let test_stats = stats::collect_stats(&python, &fixture, &pythonpath, &mutants_dir, "tests", &[])
        .expect("Stats collection should succeed");

    println!("tests_by_function: {:?}", test_stats.tests_by_function);
    println!("duration_by_test: {:?}", test_stats.duration_by_test);

    // We should have duration data for all 3 tests
    assert!(
        test_stats.duration_by_test.len() >= 3,
        "Should have duration data for at least 3 tests, got {}",
        test_stats.duration_by_test.len()
    );

    // Check that function coverage was recorded
    if !test_stats.tests_by_function.is_empty() {
        let add_tests = test_stats.tests_for_function("simple_lib.x_add");
        println!("Tests covering add: {:?}", add_tests);
    }
}

// --- run_isolated tests ---
//
// These tests exercise the isolated subprocess runner (used by --isolate and --verify-survivors).
// Each test spawns real Python processes; they require the simple_project fixture venv.

/// Build a minimal RunConfig for run_isolated tests.
fn make_run_config(python: PathBuf, paths_to_mutate: PathBuf) -> irradiate::pipeline::RunConfig {
    irradiate::pipeline::RunConfig {
        paths_to_mutate: vec![paths_to_mutate],
        tests_dir: "tests".to_string(),
        workers: 1,
        timeout_multiplier: 10.0,
        no_stats: true,
        covered_only: false,
        python,
        mutant_filter: None,
        worker_recycle_after: None,
        max_worker_memory_mb: 0,
        isolate: true,
        verify_survivors: false,
        do_not_mutate: vec![],
        fail_under: None,
        diff_ref: None,
        report: None,
        report_output: None,
        sample: None,
        sample_seed: 0,
        pytest_add_cli_args: vec![],
    }
}

/// INV-1: run_isolated returns Killed when a real mutant is caught by its test.
#[tokio::test]
async fn test_run_isolated_killed_mutant() {
    let fixture = fixture_dir();
    let python = fixture_python();
    let (_tmp, mutant_names) = generate_test_mutants();
    let harness_dir = harness::extract_harness(&fixture).expect("harness extraction");

    let add_mutant = mutant_names
        .iter()
        .find(|n| n.contains("x_add__irradiate_"))
        .expect("Should have an add mutant");

    let config = make_run_config(python, fixture.join("src"));
    let work_items = vec![WorkItem {
        mutant_name: add_mutant.clone(),
        test_ids: vec!["tests/test_simple.py::test_add".to_string()],
        estimated_duration_secs: 0.0,
        timeout_secs: 300.0,
    }];

    let results = irradiate::pipeline::run_isolated(
        &config,
        work_items,
        &harness_dir,
        _tmp.path(),
        None,
        &fixture,
    )
    .await
    .expect("run_isolated should complete");

    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].status,
        MutantStatus::Killed,
        "Add mutant must be killed by test_add: {:?}",
        results[0]
    );
}

/// INV-1: run_isolated returns Survived when a nonexistent mutant key is used
/// (trampoline falls through to the original function, so all tests pass).
#[tokio::test]
async fn test_run_isolated_survived_nonexistent_mutant() {
    let fixture = fixture_dir();
    let python = fixture_python();
    let (_tmp, _) = generate_test_mutants();
    let harness_dir = harness::extract_harness(&fixture).expect("harness extraction");

    let config = make_run_config(python, fixture.join("src"));
    let work_items = vec![WorkItem {
        mutant_name: "simple_lib.x_nonexistent__irradiate_99".to_string(),
        test_ids: vec![
            "tests/test_simple.py::test_add".to_string(),
            "tests/test_simple.py::test_is_positive".to_string(),
            "tests/test_simple.py::test_greet".to_string(),
        ],
        estimated_duration_secs: 0.0,
        timeout_secs: 300.0,
    }];

    let results = irradiate::pipeline::run_isolated(
        &config,
        work_items,
        &harness_dir,
        _tmp.path(),
        None,
        &fixture,
    )
    .await
    .expect("run_isolated should complete");

    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].status,
        MutantStatus::Survived,
        "Nonexistent mutant must survive (trampoline calls original): {:?}",
        results[0]
    );
}

/// INV-1: run_isolated processes multiple mutants in order, returning one result per work item.
#[tokio::test]
async fn test_run_isolated_multiple_items() {
    let fixture = fixture_dir();
    let python = fixture_python();
    let (_tmp, mutant_names) = generate_test_mutants();
    let harness_dir = harness::extract_harness(&fixture).expect("harness extraction");

    let add_mutant = mutant_names
        .iter()
        .find(|n| n.contains("x_add__irradiate_"))
        .expect("Should have an add mutant");

    let config = make_run_config(python, fixture.join("src"));
    let work_items = vec![
        // Killed: real mutant caught by its test
        WorkItem {
            mutant_name: add_mutant.clone(),
            test_ids: vec!["tests/test_simple.py::test_add".to_string()],
            estimated_duration_secs: 0.0,
            timeout_secs: 300.0,
        },
        // Survived: nonexistent mutant → trampoline calls original
        WorkItem {
            mutant_name: "simple_lib.x_nonexistent__irradiate_1".to_string(),
            test_ids: vec!["tests/test_simple.py::test_add".to_string()],
            estimated_duration_secs: 0.0,
            timeout_secs: 300.0,
        },
    ];

    let results = irradiate::pipeline::run_isolated(
        &config,
        work_items,
        &harness_dir,
        _tmp.path(),
        None,
        &fixture,
    )
    .await
    .expect("run_isolated should complete");

    assert_eq!(results.len(), 2, "Should have one result per work item");
    assert_eq!(results[0].status, MutantStatus::Killed, "First mutant must be killed");
    assert_eq!(results[1].status, MutantStatus::Survived, "Second mutant must survive");
}

/// INV-2 + INV-3: The verify-survivors cache correction pipeline.
///
/// When verify-survivors detects a false negative (a mutant that survived the warm
/// session but is killed in isolation), it calls force_update_entry to correct the
/// stale Survived cache entry. This test verifies the cache is updated correctly.
#[test]
fn test_verify_survivors_cache_correction_pipeline() {
    use irradiate::cache;
    use irradiate::protocol::MutantStatus;

    let tmp = tempfile::tempdir().unwrap();

    // Simulate: warm-session run stored a Survived result in the cache
    cache::store_entry(tmp.path(), "abc123def456", 0, 2.0, MutantStatus::Survived).unwrap();

    let before = cache::load_entry(tmp.path(), "abc123def456")
        .unwrap()
        .expect("Entry should exist after store");
    assert_eq!(before.status, MutantStatus::Survived, "Initial status must be Survived");

    // Simulate: verify-survivors isolated run finds the mutant is actually Killed.
    // The pipeline calls force_update_entry to correct the stale cache entry.
    cache::force_update_entry(tmp.path(), "abc123def456", 1, 0.8, MutantStatus::Killed).unwrap();

    // INV-3: Cache must now return Killed
    let after = cache::load_entry(tmp.path(), "abc123def456")
        .unwrap()
        .expect("Entry should exist after force_update");
    assert_eq!(
        after.status,
        MutantStatus::Killed,
        "INV-3: force_update_entry must flip Survived→Killed for future cache hits"
    );
    assert_eq!(after.exit_code, 1);

    // INV-3: A second store (simulating a new warm-session run) must not overwrite
    // the already-corrected Killed entry (store_entry is immutable).
    cache::store_entry(tmp.path(), "abc123def456", 0, 5.0, MutantStatus::Survived).unwrap();
    let still_killed = cache::load_entry(tmp.path(), "abc123def456")
        .unwrap()
        .expect("Entry must still exist");
    assert_eq!(
        still_killed.status,
        MutantStatus::Killed,
        "store_entry must not overwrite an existing corrected entry"
    );
}
