//! TEMPORARY: probe wt2html output for a wikitext snippet.

mod harness;

use harness::ParserTestCase;

#[test]
fn wt2html_probe() {
    let wt = std::env::var("WT").unwrap_or_default();
    let mut test = ParserTestCase {
        description: "probe".to_string(),
        options_raw: String::new(),
        options: Default::default(),
        config_raw: String::new(),
        wikitext: wt.clone(),
        html_parsoid: Some("PLACEHOLDER".to_string()),
        html_php: None,
        html_parsoid_lang: None,
        wikitext_edited: None,
        line_number: 0,
        parsoid_only: true,
    };
    test.options
        .insert("parsoid".to_string(), "wt2html".to_string());
    let tf = harness::ParserTestFile {
        path: "probe".to_string(),
        ..Default::default()
    };
    match harness::run_single_test_public(&test, &tf) {
        harness::TestResult::Fail { actual, .. } => {
            eprintln!("HTML: {}", actual.replace('\n', "\\n"))
        }
        other => eprintln!("RESULT: {other:?}"),
    }
}
