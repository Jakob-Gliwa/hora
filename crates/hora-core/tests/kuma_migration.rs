//! Exercise the Python migration's emitted jq against the production evaluator.
use hora_core::{
    assertion::{Comparison, JqAssert},
    config::Compiled,
};
use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    query: String,
    expected: String,
    body: serde_json::Value,
    up: bool,
    #[serde(default)]
    operator: Comparison,
}

#[test]
fn translated_kuma_assertions_preserve_verdicts() {
    let bytes = if let Ok(path) = std::env::var("HORA_MIGRATION_CASES") {
        std::fs::read(path).unwrap()
    } else {
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/tests/migration_cases.py");
        let result = std::process::Command::new("python3")
            .arg(script)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "synthetic fixture generation failed"
        );
        result.stdout
    };
    let cases: Vec<Case> = serde_json::from_slice(&bytes).unwrap();
    assert!(!cases.is_empty());
    for (index, case) in cases.into_iter().enumerate() {
        let assertion = JqAssert {
            query: Compiled::parse(&case.query).expect("translated jq must compile"),
            expected: Some(case.expected),
            operator: case.operator,
        };
        assert_eq!(
            assertion
                .check(&serde_json::to_vec(&case.body).unwrap())
                .is_none(),
            case.up,
            "case {index}"
        );
    }
}
