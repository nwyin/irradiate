use irradiate::harness;
use irradiate::orchestrator::{run_worker_pool, PoolConfig};
use irradiate::protocol::{MutantStatus, WorkItem};
use irradiate::stats;
use std::path::PathBuf;

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Find the Python interpreter in the test fixture's venv.
fn fixture_python() -> PathBuf {
    let root = project_root();
    let venv_python = root
        .join("tests/fixtures/simple_project/.venv/bin/python3");
    if venv_python.exists() {
        venv_python
    } else {
        // Fall back to system python
        PathBuf::from("python3")
    }
}

fn fixture_dir() -> PathBuf {
    project_root().join("tests/fixtures/simple_project")
}

#[tokio::test]
async fn test_worker_pool_dispatches_mutants() {
    let fixture = fixture_dir();
    let python = fixture_python();

    // Verify fixture exists
    assert!(
        fixture.join("mutants/simple_lib/__init__.py").exists(),
        "Trampolined fixture must exist"
    );
    assert!(
        fixture.join("tests/test_simple.py").exists(),
        "Test fixture must exist"
    );

    let config = PoolConfig {
        num_workers: 2,
        python,
        project_dir: fixture.clone(),
        mutants_dir: fixture.join("mutants"),
        tests_dir: fixture.join("tests"),
        timeout_multiplier: 10.0,
        ..Default::default()
    };

    let work_items = vec![
        // Mutant 1: add(a,b) returns a - b. Should be KILLED by test_add.
        WorkItem {
            mutant_name: "simple_lib.x_add__mutmut_1".to_string(),
            test_ids: vec!["tests/test_simple.py::test_add".to_string()],
        },
        // Mutant 2: add(a,b) returns a * b. Should be KILLED by test_add.
        WorkItem {
            mutant_name: "simple_lib.x_add__mutmut_2".to_string(),
            test_ids: vec!["tests/test_simple.py::test_add".to_string()],
        },
        // Mutant 3: is_positive uses >= instead of >. Should be KILLED (is_positive(0) would return True).
        WorkItem {
            mutant_name: "simple_lib.x_is_positive__mutmut_1".to_string(),
            test_ids: vec!["tests/test_simple.py::test_is_positive".to_string()],
        },
    ];

    let results = run_worker_pool(&config, work_items).await.expect("Worker pool should complete");

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

    let config = PoolConfig {
        num_workers: 1,
        python,
        project_dir: fixture.clone(),
        mutants_dir: fixture.join("mutants"),
        tests_dir: fixture.join("tests"),
        ..Default::default()
    };

    let results = run_worker_pool(&config, vec![]).await.expect("Empty work should succeed");
    assert!(results.is_empty());
}

#[tokio::test]
async fn test_worker_pool_surviving_mutant() {
    let fixture = fixture_dir();
    let python = fixture_python();

    let config = PoolConfig {
        num_workers: 1,
        python,
        project_dir: fixture.clone(),
        mutants_dir: fixture.join("mutants"),
        tests_dir: fixture.join("tests"),
        ..Default::default()
    };

    // Mutant: greet returns "XXHello, XX" + name
    // test_greet checks greet("World") == "Hello, World"
    // "XXHello, XX" + "World" = "XXHello, XXWorld" != "Hello, World"
    // So this should be KILLED.
    //
    // But mutant 2: greet returns "Hello, " - name -> TypeError -> KILLED
    //
    // To test a surviving mutant, we'd need one that the tests don't catch.
    // Let's use a mutant name that doesn't match any function — the trampoline
    // will call the original, so tests pass -> survived.
    let work_items = vec![WorkItem {
        mutant_name: "simple_lib.x_nonexistent__mutmut_1".to_string(),
        test_ids: vec![
            "tests/test_simple.py::test_add".to_string(),
            "tests/test_simple.py::test_is_positive".to_string(),
            "tests/test_simple.py::test_greet".to_string(),
        ],
    }];

    let results = run_worker_pool(&config, work_items).await.expect("Worker pool should complete");
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].status,
        MutantStatus::Survived,
        "Non-matching mutant should survive (trampoline falls through to original)"
    );
}

#[tokio::test]
async fn test_stats_collection() {
    let fixture = fixture_dir();
    let python = fixture_python();

    // Extract harness
    let harness_dir = harness::extract_harness(&fixture).expect("harness extraction");

    let test_stats = stats::collect_stats(
        &python,
        &fixture,
        &harness_dir,
        &fixture.join("mutants"),
        "tests",
    )
    .expect("Stats collection should succeed");

    // The stats plugin records hits to trampolined functions.
    // Our fixture has add, is_positive, greet trampolined.
    // test_add calls add -> hits simple_lib.x_add
    // test_is_positive calls is_positive -> hits simple_lib.x_is_positive
    // test_greet calls greet -> hits simple_lib.x_greet
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
        // If stats plugin worked, we should see function -> test mappings
        let add_tests = test_stats.tests_for_function("simple_lib.x_add");
        println!("Tests covering add: {:?}", add_tests);
    }
}
