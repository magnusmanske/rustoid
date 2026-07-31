//! Debug tool: runs all fixtures and prints failure details.
//! Run with: cargo test -p rustoid-core --test debug_failures -- --nocapture

use rustoid_core::test_harness;

#[test]
fn debug_all_failures() {
    let fixture_dir = std::path::Path::new("tests/fixtures");
    let entries = std::fs::read_dir(fixture_dir).expect("fixtures dir exists");

    for entry in entries {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().map_or(true, |e| e != "txt") {
            continue;
        }
        let fname = path.file_name().unwrap().to_string_lossy().to_string();
        let test_file = test_harness::parse_test_file(&path).unwrap();
        eprintln!("\n=== {fname} ({}) ===", test_file.tests.len());
        for test in &test_file.tests {
            let result = test_harness::run_single_test_public(test, &test_file);
            match &result {
                test_harness::TestResult::Pass => {}
                test_harness::TestResult::Fail {
                    expected,
                    actual,
                    diff_hint,
                } => {
                    eprintln!("  FAIL: {}", test.description);
                    eprintln!("    diff: {diff_hint}");
                    eprintln!("    expected: {expected}");
                    eprintln!("    actual:   {actual}");
                }
                test_harness::TestResult::Skip(reason) => {
                    eprintln!("  SKIP: {} ({})", test.description, reason);
                }
                test_harness::TestResult::Error(e) => {
                    eprintln!("  ERROR: {} ({})", test.description, e);
                }
            }
        }
    }
}
