//! Reproducible synthetic assertion microbenchmark; no broker or private data.
//! `cargo test -p hora-core --release --lib benchmark_mqtt_content -- --ignored --nocapture`

use super::*;
use std::hint::black_box;

fn spec(jq: Option<&str>, keyword: Option<&str>) -> MqttSpec {
    MqttSpec {
        broker: "synthetic".into(),
        topic: "synthetic/state".into(),
        max_age_secs: Some(900),
        keyword: keyword.map(|text| crate::config::KeywordAssert {
            text: text.into(),
            invert: false,
        }),
        json: None,
        jq: jq.map(|query| crate::assertion::JqAssert {
            query: crate::config::Compiled::parse(query).unwrap(),
            expected: None,
            operator: crate::assertion::Comparison::Eq,
        }),
        max_body_kb: None,
    }
}

fn measure(spec: &MqttSpec, payload: &[u8], reuse: u32, iterations: u32, cached: bool) -> f64 {
    let now = Instant::now();
    let mut message = Message::new(payload, now);
    let mut memo = None;
    let start = std::time::Instant::now();
    for i in 0..iterations {
        if i.is_multiple_of(reuse) {
            message = Message::new(black_box(payload), now);
        }
        let outcome = evaluate(
            black_box(spec),
            Some(black_box(&message)),
            now,
            if cached { Some(&mut memo) } else { None },
        );
        black_box(outcome);
    }
    start.elapsed().as_secs_f64() * 1e9 / f64::from(iterations)
}

#[test]
#[ignore = "manual release microbenchmark, synthetic inputs only"]
fn benchmark_mqtt_content() {
    let large = serde_json::json!({
        "values": (0..512).collect::<Vec<_>>(),
        "padding": "x".repeat(2048),
    })
    .to_string();
    let cases = [
        (
            "keyword",
            spec(None, Some("online")),
            b"online".as_slice(),
            500_000,
        ),
        (
            "jq_scalar",
            spec(Some(". > 100"), None),
            b"101".as_slice(),
            100_000,
        ),
        (
            "jq_object",
            spec(Some(".value > 10"), None),
            br#"{"value":11,"state":"online"}"#.as_slice(),
            100_000,
        ),
        (
            "jq_array",
            spec(Some("[.values[] | select(. >= 0)] | length == 512"), None),
            large.as_bytes(),
            2_000,
        ),
    ];
    println!("case,bytes,checks_per_message,uncached_ns,cached_ns,speedup");
    for (name, spec, payload, iterations) in cases {
        for reuse in [1, 5, 50] {
            // Warm both paths, then alternate their order across five samples.
            measure(&spec, payload, reuse, 100, false);
            measure(&spec, payload, reuse, 100, true);
            let mut baseline = Vec::new();
            let mut optimized = Vec::new();
            for sample in 0..5 {
                for cached in [sample % 2 == 0, sample % 2 != 0] {
                    let ns = measure(&spec, payload, reuse, iterations, cached);
                    if cached {
                        optimized.push(ns);
                    } else {
                        baseline.push(ns);
                    }
                }
            }
            baseline.sort_by(f64::total_cmp);
            optimized.sort_by(f64::total_cmp);
            println!(
                "{name},{},{reuse},{:.1},{:.1},{:.2}",
                payload.len(),
                baseline[2],
                optimized[2],
                baseline[2] / optimized[2]
            );
        }
    }
    println!(
        "cache_bytes_per_monitor={}",
        std::mem::size_of::<Option<ContentCache>>()
    );
}
