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
        codegen::mutate_file(&source, "simple_lib").expect("fixture should produce mutations");

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
        codegen::mutate_file(&source, module_name).expect("fixture should produce mutations");

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

    let results = run_worker_pool(&config, work_items)
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

    let results = run_worker_pool(&config, vec![])
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

    let results = run_worker_pool(&config, work_items)
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

    // recycle_after=1: worker is replaced after every single mutant
    let config = PoolConfig {
        num_workers: 1,
        python,
        project_dir: fixture.clone(),
        mutants_dir: _tmp.path().to_path_buf(),
        tests_dir: fixture.join("tests"),
        timeout_multiplier: 10.0,
        worker_recycle_after: 1,
        ..Default::default()
    };

    let results = run_worker_pool(&config, work_items)
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
        worker_recycle_after: 0, // disabled
        ..Default::default()
    };

    let results = run_worker_pool(&config, work_items)
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
        worker_recycle_after: 0,
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

    let results = run_worker_pool(&config, work_items)
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
    let pythonpath = pipeline::build_pythonpath(&harness_dir, &project.path().join("src"));
    let mutants_dir = _tmp.path().to_path_buf();
    let test_stats =
        stats::collect_stats(&python, project.path(), &pythonpath, &mutants_dir, "tests")
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
        worker_recycle_after: 0,
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

    let results = run_worker_pool(&config, work_items)
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
    let pythonpath = pipeline::build_pythonpath(&harness_dir, &project.path().join("src"));
    let mutants_dir = _tmp.path().to_path_buf();

    // Use stats collection to discover the actual test ID format (pytest nodeids vary).
    let test_stats =
        stats::collect_stats(&python, project.path(), &pythonpath, &mutants_dir, "tests")
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
        worker_recycle_after: 0, // No recycling — we test module restore, not recycling
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

    let results = run_worker_pool(&config, work_items)
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

/// INV-2: Trampolined function dispatch still works after module restore.
///
/// After restore, the trampoline dict and variant functions must be intact.
/// We run a real mutant (not nonexistent) and verify it is still killed by the test.
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
        worker_recycle_after: 0, // same worker for both runs
        ..Default::default()
    };

    let results = run_worker_pool(&config, work_items)
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
        "INV-2: trampoline must still dispatch mutant correctly after module restore"
    );
}

#[tokio::test]
async fn test_stats_collection() {
    let fixture = fixture_dir();
    let python = fixture_python();
    let (_tmp, _) = generate_test_mutants();

    // Extract harness
    let harness_dir = harness::extract_harness(&fixture).expect("harness extraction");

    let mutants_dir = _tmp.path().to_path_buf();
    let pythonpath = pipeline::build_pythonpath(&harness_dir, &fixture.join("src"));
    let test_stats = stats::collect_stats(&python, &fixture, &pythonpath, &mutants_dir, "tests")
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
