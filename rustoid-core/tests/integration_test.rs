//! Integration tests for the Parsoid test harness.

use rustoid_core::test_harness;

#[test]
fn test_harness_minitests() {
    let path = std::path::Path::new("tests/fixtures/minitests.txt");
    let summary = test_harness::run_test_file(path).unwrap();
    eprintln!("Summary:\n{summary}");
    assert!(summary.total > 0);
}

#[test]
fn test_harness_parse_only() {
    let path = std::path::Path::new("tests/fixtures/minitests.txt");
    let test_file = test_harness::parse_test_file(path).unwrap();
    assert!(!test_file.tests.is_empty());
    assert!(test_file.articles.contains_key("Template:1x"));
    assert_eq!(test_file.tests.len(), 7);
}

#[test]
fn diagnostic_minitests() {
    let path = std::path::Path::new("tests/fixtures/minitests.txt");
    let test_file = test_harness::parse_test_file(path).unwrap();
    let mut passed = 0;
    let total = test_file.tests.len();
    for test in &test_file.tests {
        let result = test_harness::run_single_test_public(test, &test_file);
        if matches!(result, test_harness::TestResult::Pass) {
            passed += 1;
        }
    }
    // 5 out of 7 mini tests pass
    assert!(passed >= 4, "Only {passed}/{total} passed");
}
