use irradiate::codegen;
use irradiate::harness;
use irradiate::orchestrator::{run_worker_pool, PoolConfig};
use irradiate::pipeline;
use irradiate::protocol::{MutantStatus, WorkItem};
use irradiate::stats;
use std::path::PathBuf;

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
    let source = std::fs::read_to_string(fixture_dir().join("src/simple_lib/__init__.py")).unwrap();
    let mutated =
        codegen::mutate_file(&source, "simple_lib").expect("fixture should produce mutations");

    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("simple_lib")).unwrap();
    std::fs::write(tmp.path().join("simple_lib/__init__.py"), &mutated.source).unwrap();

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
        .find(|n| n.contains("x_add__mutmut_"))
        .expect("Should have an add mutant");

    // Pick an is_positive mutant (should be killed by test_is_positive)
    let is_pos_mutant = mutant_names
        .iter()
        .find(|n| n.contains("x_is_positive__mutmut_"))
        .expect("Should have an is_positive mutant");

    // Pick a greet mutant (should be killed by test_greet)
    let greet_mutant = mutant_names
        .iter()
        .find(|n| n.contains("x_greet__mutmut_"))
        .expect("Should have a greet mutant");

    let work_items = vec![
        WorkItem {
            mutant_name: add_mutant.clone(),
            test_ids: vec!["tests/test_simple.py::test_add".to_string()],
        },
        WorkItem {
            mutant_name: is_pos_mutant.clone(),
            test_ids: vec!["tests/test_simple.py::test_is_positive".to_string()],
        },
        WorkItem {
            mutant_name: greet_mutant.clone(),
            test_ids: vec!["tests/test_simple.py::test_greet".to_string()],
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
        mutant_name: "simple_lib.x_nonexistent__mutmut_1".to_string(),
        test_ids: vec![
            "tests/test_simple.py::test_add".to_string(),
            "tests/test_simple.py::test_is_positive".to_string(),
            "tests/test_simple.py::test_greet".to_string(),
        ],
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
        .find(|n| n.contains("x_add__mutmut_"))
        .expect("Should have an add mutant");

    let is_pos_mutant = mutant_names
        .iter()
        .find(|n| n.contains("x_is_positive__mutmut_"))
        .expect("Should have an is_positive mutant");

    let greet_mutant = mutant_names
        .iter()
        .find(|n| n.contains("x_greet__mutmut_"))
        .expect("Should have a greet mutant");

    let work_items = vec![
        WorkItem {
            mutant_name: add_mutant.clone(),
            test_ids: vec!["tests/test_simple.py::test_add".to_string()],
        },
        WorkItem {
            mutant_name: is_pos_mutant.clone(),
            test_ids: vec!["tests/test_simple.py::test_is_positive".to_string()],
        },
        WorkItem {
            mutant_name: greet_mutant.clone(),
            test_ids: vec!["tests/test_simple.py::test_greet".to_string()],
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
        .find(|n| n.contains("x_add__mutmut_"))
        .expect("Should have an add mutant");

    let work_items = vec![WorkItem {
        mutant_name: add_mutant.clone(),
        test_ids: vec!["tests/test_simple.py::test_add".to_string()],
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
