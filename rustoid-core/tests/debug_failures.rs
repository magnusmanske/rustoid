//! Debug tool: runs all fixtures and prints failure details.
//! Run with: cargo test -p rustoid-core --test debug_failures -- --nocapture

mod harness;

#[test]
fn debug_all_failures() {
    let fixture_dir = std::path::Path::new("tests/fixtures");
    let entries = std::fs::read_dir(fixture_dir).expect("fixtures dir exists");

    for entry in entries {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "txt") {
            continue;
        }
        let fname = path.file_name().unwrap().to_string_lossy().to_string();
        let test_file = harness::parse_test_file(&path).unwrap();
        eprintln!("\n=== {fname} ({}) ===", test_file.tests.len());
        for test in &test_file.tests {
            let result = harness::run_single_test_public(test, &test_file);
            match &result {
                harness::TestResult::Pass => {}
                harness::TestResult::Fail {
                    expected,
                    actual,
                    diff_hint,
                } => {
                    eprintln!("  FAIL: {}", test.description);
                    eprintln!("    diff: {diff_hint}");
                    eprintln!("    expected: {expected}");
                    eprintln!("    actual:   {actual}");
                }
                harness::TestResult::Skip(reason) => {
                    eprintln!("  SKIP: {} ({})", test.description, reason);
                }
                harness::TestResult::Error(e) => {
                    eprintln!("  ERROR: {} ({})", test.description, e);
                }
            }
        }
    }
}
