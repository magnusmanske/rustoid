//! Integration tests for the Parsoid test harness.

mod harness;

fn run_all_fixtures() {
    let fixture_dir = std::path::Path::new("tests/fixtures");
    let entries = std::fs::read_dir(fixture_dir).expect("fixtures dir exists");
    let mut total = 0;
    let mut passed = 0;
    let mut file_results = Vec::new();

    for entry in entries {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "txt") {
            continue;
        }
        let summary = harness::run_test_file(&path).unwrap();
        total += summary.total;
        passed += summary.passed;
        let fname = path.file_name().unwrap().to_string_lossy().to_string();
        file_results.push(format!("  {fname}: {}/{}", summary.passed, summary.total));
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
    assert!(
        passed >= 5,
        "Only {passed}/{total} tests passed (below minimum of 5)"
    );
}

#[test]
fn test_all_parsoid_fixtures() {
    run_all_fixtures();
}

#[test]
fn test_fixture_summary() {
    run_all_fixtures();
}

#[test]
fn test_harness_parse_only() {
    let path = std::path::Path::new("tests/fixtures/minitests.txt");
    let test_file = harness::parse_test_file(path).unwrap();
    assert!(!test_file.tests.is_empty());
}
