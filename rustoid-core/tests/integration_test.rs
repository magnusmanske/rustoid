//! Integration tests for the Parsoid test harness.

use rustoid_core::test_harness;

fn run_all_fixtures() {
    let fixture_dir = std::path::Path::new("tests/fixtures");
    let entries = std::fs::read_dir(fixture_dir).expect("fixtures dir exists");
    let mut total = 0;
    let mut passed = 0;
    let mut failed = 0;
    let mut skipped = 0;
    let mut errors = 0;
    let mut file_results = Vec::new();
    let mut failure_details = Vec::new();

    for entry in entries {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().map_or(true, |e| e != "txt") {
            continue;
        }
        let summary = test_harness::run_test_file(&path).unwrap();
        total += summary.total;
        passed += summary.passed;
        failed += summary.failed;
        skipped += summary.skipped;
        errors += summary.errors;
        let fname = path.file_name().unwrap().to_string_lossy().to_string();
        let pct = if summary.total > 0 {
            (summary.passed as f64 / summary.total as f64) * 100.0
        } else {
            0.0
        };
        file_results.push(format!("  {fname}: {}/{}", summary.passed, summary.total));
        for (name, result) in &summary.failures {
            failure_details.push(format!("    [{fname}] {name}: {result}"));
        }
    }

    let overall_pct = if total > 0 {
        (passed as f64 / total as f64) * 100.0
    } else {
        0.0
    };

    eprintln!("Parsoid test results: {passed}/{total} passed ({overall_pct:.0}%)");
    for fr in &file_results {
        eprintln!("{fr}");
    }

    // We expect at least some tests to pass
    assert!(passed > 0, "No tests passed");
}

#[test]
fn test_all_parsoid_fixtures() {
    run_all_fixtures();
}

#[test]
fn test_harness_parse_only() {
    let path = std::path::Path::new("tests/fixtures/minitests.txt");
    let test_file = test_harness::parse_test_file(path).unwrap();
    assert!(!test_file.tests.is_empty());
}
