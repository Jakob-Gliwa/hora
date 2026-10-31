//! jq expressions compiled once at configuration load and shared by HTTP/MQTT.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};

use jaq_core::{
    Ctx, Vars, data,
    load::{Arena, File, Loader},
    unwrap_valr,
};
use jaq_json::Val;
use serde::Deserialize;

use crate::config::{Compiled, ParseField};

/// Shared keyword semantics for HTTP bodies and MQTT payloads.
pub(crate) fn check_keyword(keyword: &crate::config::KeywordAssert, body: &[u8]) -> Option<String> {
    let found = String::from_utf8_lossy(body).contains(keyword.text.as_str());
    (found == keyword.invert).then(|| {
        if keyword.invert {
            format!("keyword present: {}", keyword.text)
        } else {
            format!("keyword missing: {}", keyword.text)
        }
    })
}

type Filter = jaq_core::Filter<data::JustLut<Val>>;

// Repeated monitor expressions share bytecode, including across live reloads.
// Weak entries never keep a removed program alive; prune dead keys on misses.
static PROGRAMS: LazyLock<Mutex<HashMap<String, Weak<Filter>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone)]
pub struct JqProgram(Arc<Filter>);

impl ParseField for JqProgram {
    fn parse_field(raw: &str) -> Result<Self, String> {
        let mut programs = PROGRAMS.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(program) = programs.get(raw).and_then(Weak::upgrade) {
            return Ok(Self(program));
        }
        programs.retain(|_, program| program.strong_count() > 0);
        // No env, debug, filesystem, module loader or subprocess capabilities.
        let defs = jaq_core::defs()
            .chain(jaq_std::defs().filter(|def| {
                matches!(
                    def.name,
                    "null"
                        | "isboolean"
                        | "isnumber"
                        | "isstring"
                        | "isarray"
                        | "isobject"
                        | "nan"
                        | "infinite"
                        | "isnan"
                        | "isinfinite"
                        | "isfinite"
                        | "isnormal"
                        | "abs"
                        | "type"
                        | "values"
                        | "nulls"
                        | "booleans"
                        | "numbers"
                        | "finites"
                        | "normals"
                        | "strings"
                        | "arrays"
                        | "objects"
                        | "iterables"
                        | "scalars"
                        | "add"
                        | "min_by"
                        | "max_by"
                        | "min"
                        | "max"
                        | "unique_by"
                        | "unique"
                        | "pick"
                        | "keys"
                        | "flatten"
                )
            }))
            .chain(jaq_json::defs());
        let funs = jaq_core::funs()
            .chain(jaq_std::base_funs())
            .chain(jaq_json::funs());
        let arena = Arena::default();
        let loader = Loader::new(defs);
        let modules = loader
            .load(
                &arena,
                File {
                    code: raw,
                    path: (),
                },
            )
            .map_err(|_| "invalid jq syntax or unavailable module".to_owned())?;
        let filter = jaq_core::Compiler::default()
            .with_funs(funs)
            .compile(modules)
            .map_err(|_| "invalid jq expression or unavailable function".to_owned())?;
        let filter = Arc::new(filter);
        programs.insert(raw.to_owned(), Arc::downgrade(&filter));
        Ok(Self(filter))
    }
}

impl JqProgram {
    /// Require exactly one scalar result; never collect an unbounded output stream.
    ///
    /// # Errors
    /// Returns a payload-free error for invalid JSON, evaluation failure or
    /// anything other than exactly one non-null scalar output.
    pub fn evaluate(&self, body: &[u8]) -> Result<serde_json::Value, &'static str> {
        let input: Val = serde_json::from_slice(body).map_err(|_| "payload is not valid JSON")?;
        let ctx = Ctx::<data::JustLut<Val>>::new(&self.0.lut, Vars::new([]));
        let mut output = self.0.id.run((ctx, input)).map(unwrap_valr);
        let value = output
            .next()
            .ok_or("jq produced no value")?
            .map_err(|_| "jq evaluation failed")?;
        if output.next().is_some() {
            return Err("jq must produce exactly one value");
        }
        if matches!(value, Val::Null | Val::Arr(_) | Val::Obj(_)) {
            return Err("jq must produce a non-null scalar");
        }
        let value = serde_json::from_str::<serde_json::Value>(&value.to_string())
            .map_err(|_| "jq result is not JSON")?;
        if value.is_null() || value.is_array() || value.is_object() {
            return Err("jq must produce a non-null scalar");
        }
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
pub enum Comparison {
    #[default]
    #[serde(rename = "==")]
    Eq,
    #[serde(rename = "!=")]
    Ne,
    #[serde(rename = "<")]
    Lt,
    #[serde(rename = "<=")]
    Le,
    #[serde(rename = ">")]
    Gt,
    #[serde(rename = ">=")]
    Ge,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JqAssert {
    pub query: Compiled<JqProgram>,
    pub expected: Option<String>,
    pub operator: Comparison,
}

impl JqAssert {
    #[must_use]
    #[allow(clippy::float_cmp)] // Exact equality is the configured assertion, not an approximation.
    pub fn check(&self, body: &[u8]) -> Option<String> {
        let value = match self.query.evaluate(body) {
            Ok(value) => value,
            Err(error) => return Some(error.to_owned()),
        };
        let expected = self.expected.as_deref().unwrap_or("true");
        let text = value
            .as_str()
            .map_or_else(|| value.to_string(), str::to_owned);
        let numeric = text
            .parse::<f64>()
            .ok()
            .zip(expected.parse::<f64>().ok())
            .filter(|(a, b)| a.is_finite() && b.is_finite());
        let equal = numeric.map_or(text == expected, |(a, b)| a == b);
        let passed = match self.operator {
            Comparison::Eq => equal,
            Comparison::Ne => !equal,
            Comparison::Lt => numeric.is_some_and(|(a, b)| a < b),
            Comparison::Le => numeric.is_some_and(|(a, b)| a <= b),
            Comparison::Gt => numeric.is_some_and(|(a, b)| a > b),
            Comparison::Ge => numeric.is_some_and(|(a, b)| a >= b),
        };
        (!passed).then(|| "jq assertion failed".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_queries_share_compiled_bytecode() {
        let first = JqProgram::parse_field(".shared == true").unwrap();
        let second = JqProgram::parse_field(".shared == true").unwrap();
        assert!(Arc::ptr_eq(&first.0, &second.0));
    }

    fn check(query: &str, body: &str, expected: &str) -> bool {
        JqAssert {
            query: Compiled::parse(query).unwrap(),
            expected: Some(expected.into()),
            operator: Comparison::Eq,
        }
        .check(body.as_bytes())
        .is_none()
    }

    #[test]
    fn jq_scalar_ranges_counts_and_numeric_strings() {
        for (query, up, down, expected) in [
            (". > 100", "101", "99", "true"),
            ("type == \"number\" and . >= 0 and . <= 7", "7", "8", "true"),
            (
                "[.[] | select(.source == \"a\" or .source == \"b\")] | length",
                r#"[{"source":"c"}]"#,
                r#"[{"source":"a"}]"#,
                "0",
            ),
            (
                ".data.result[0].value[1] | tonumber | round",
                r#"{"data":{"result":[{"value":[0,"2.4"]}]}}"#,
                r#"{"data":{"result":[{"value":[0,"3.4"]}]}}"#,
                "2",
            ),
            (
                ".data.result[0].value[1] | tonumber | . * 10 | round | . / 10",
                r#"{"data":{"result":[{"value":[0,"2.44"]}]}}"#,
                r#"{"data":{"result":[{"value":[0,"2.55"]}]}}"#,
                "2.4",
            ),
            (
                "if .results[0].series[0].values[0][1] > 0 then 1 else 0 end",
                r#"{"results":[{"series":[{"values":[[0,1]]}]}]}"#,
                "{}",
                "1",
            ),
        ] {
            assert!(check(query, up, expected), "{query}");
            assert!(!check(query, down, expected), "{query}");
        }
    }

    #[test]
    fn jq_no_data_errors_and_multiple_outputs_fail_closed() {
        for (query, body) in [
            (".missing", "{}"),
            (".[]", "[]"),
            (".[]", "[1,2]"),
            ("tonumber", "\"bad\""),
            (".", "invalid"),
            (".", "null"),
        ] {
            assert!(!check(query, body, "true"));
        }
        assert!(JqProgram::parse_field("env").is_err());
        assert!(JqProgram::parse_field("import \"secret\" as x; x::read").is_err());
        assert!(JqProgram::parse_field("(").is_err());
    }

    #[test]
    fn jq_comparison_operators() {
        for (operator, expected, pass) in [
            (Comparison::Eq, "2.0", true),
            (Comparison::Ne, "3", true),
            (Comparison::Lt, "3", true),
            (Comparison::Le, "2", true),
            (Comparison::Gt, "1", true),
            (Comparison::Ge, "2", true),
            (Comparison::Lt, "1", false),
            (Comparison::Gt, "bad", false),
        ] {
            let assertion = JqAssert {
                query: Compiled::parse(".").unwrap(),
                expected: Some(expected.into()),
                operator,
            };
            assert_eq!(assertion.check(b"2").is_none(), pass);
        }
    }
}
