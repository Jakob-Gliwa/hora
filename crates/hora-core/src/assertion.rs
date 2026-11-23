//! jq assertions (`jq` / `jq_expected` / `jq_operator`): compiled once at
//! load with jaq, shared by every monitor that spells the same expression,
//! and evaluated without environment, filesystem or module access.
//!
//! The expression is configuration; the payload it runs on is not. What the
//! payload controls is bounded: its nesting (`serde_json` stops at 128
//! levels, on input and in `fromjson`) and the outputs pulled (two at most).

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};

use jaq_core::load::lex::{StrPart, Tok, Token};
use jaq_core::load::{Arena, File, Lexer, Loader};
use jaq_core::{Ctx, Cv, ValXs, Vars, data, native};
use jaq_json::Val;
use serde::Deserialize;

use crate::config::{Compiled, ParseField};

type Data = data::JustLut<Val>;
type Filter = jaq_core::Filter<Data>;

// Repeated monitor expressions share bytecode, including across live reloads.
// Weak entries never keep a removed program alive; prune dead keys on misses.
static PROGRAMS: LazyLock<Mutex<HashMap<String, Weak<Filter>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Native functions left out: `halt` would exit the process, and jaq's
/// `fromjson` (which `tonumber` calls) parses without a depth limit, so a
/// deeply nested string in a payload would overflow the stack. It is
/// replaced by [`fromjson`].
const NATIVE_OUT: &[&str] = &["halt", "fromjson"];

/// `fromjson` with `serde_json`'s nesting limit.
fn fromjson(cv: Cv<'_, Data>) -> ValXs<'_, Val> {
    native::bome(match cv.1 {
        Val::TStr(text) => {
            serde_json::from_slice(&text).map_err(|_| jaq_core::Error::str("cannot parse as JSON"))
        }
        other => Err(jaq_core::Error::typ(other, "string")),
    })
}

/// Whether the tokens define a function. A recursive one can overflow the
/// stack, which aborts the process rather than failing the check.
fn defines_function(tokens: &[Token<&str>]) -> bool {
    tokens.iter().any(|Token(text, tok)| match tok {
        Tok::Word => *text == "def",
        Tok::Block(inner) => defines_function(inner),
        Tok::Str(parts) => parts.iter().any(|part| match part {
            StrPart::Term(term) => defines_function(std::slice::from_ref(term)),
            StrPart::Str(_) | StrPart::Char(_) => false,
        }),
        _ => false,
    })
}

/// A compiled jq filter, shared (by its text) with every other monitor that
/// spells the same expression.
#[derive(Clone)]
pub struct JqProgram(Arc<Filter>);

impl ParseField for JqProgram {
    fn parse_field(raw: &str) -> Result<Self, String> {
        let mut programs = PROGRAMS.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(program) = programs.get(raw).and_then(Weak::upgrade) {
            return Ok(Self(program));
        }
        programs.retain(|_, program| program.strong_count() > 0);
        if Lexer::new(raw)
            .lex()
            .is_ok_and(|tokens| defines_function(&tokens))
        {
            return Err("function definitions (def) are not available".to_owned());
        }
        // No env, debug, halt, filesystem, module loader or subprocess capabilities.
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
            .chain(jaq_json::funs())
            .filter(|(name, ..)| !NATIVE_OUT.contains(name))
            .chain([native::run::<Data>((
                "fromjson",
                native::v(0),
                fromjson as jaq_core::RunPtr<Data>,
            ))]);
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
    /// Whether both run the same compiled program.
    #[cfg(test)]
    pub(crate) fn shares_program_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// Require exactly one scalar result; never collect an unbounded output stream.
    ///
    /// # Errors
    /// Returns a payload-free error for invalid JSON, evaluation failure or
    /// anything other than exactly one non-null scalar output.
    pub fn evaluate(&self, body: &[u8]) -> Result<serde_json::Value, &'static str> {
        let input: Val = serde_json::from_slice(body).map_err(|_| "payload is not valid JSON")?;
        let ctx = Ctx::<Data>::new(&self.0.lut, Vars::new([]));
        let mut output = self.0.id.run((ctx, input));
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

/// `jq_operator`: how the jq result is compared with `jq_expected`.
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

impl Comparison {
    /// The operator as written in the config.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Eq => "==",
            Self::Ne => "!=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
        }
    }
}

/// `jq` / `jq_expected` / `jq_operator`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JqAssert {
    pub query: Compiled<JqProgram>,
    /// The value the result is compared with; unset = `true`.
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

    #[test]
    fn tonumber_on_numeric_strings_is_unchanged() {
        let run = |query: &str, body: &str| {
            JqProgram::parse_field(query)
                .unwrap()
                .evaluate(body.as_bytes())
        };
        for (body, expected) in [
            (r#""42""#, serde_json::json!(42)),
            (r#""3.5""#, serde_json::json!(3.5)),
            (r#""-1""#, serde_json::json!(-1)),
            (r#""1e3""#, serde_json::json!(1000.0)),
        ] {
            assert_eq!(run("tonumber", body), Ok(expected), "{body}");
        }
        assert!(check("tonumber == 42", r#""42""#, "true"));
        assert!(check("tonumber", r#""1e3""#, "1000"));
        assert_eq!(
            run(".a | fromjson | .b", r#"{"a":"{\"b\":3}"}"#),
            Ok(serde_json::json!(3))
        );
    }

    #[test]
    fn deeply_nested_strings_fail_without_overflowing_the_stack() {
        let run = |query: &str, body: &str| {
            JqProgram::parse_field(query)
                .unwrap()
                .evaluate(body.as_bytes())
        };
        let nested = format!(
            r#"{{"a":"{}1{}"}}"#,
            "[".repeat(100_000),
            "]".repeat(100_000)
        );
        assert!(run(".a | tonumber", &nested).is_err());
        assert!(run(".a | fromjson | length", &nested).is_err());
    }

    #[test]
    fn halt_def_and_the_environment_are_refused_at_load() {
        for query in [
            "env",
            "$ENV",
            "now",
            "input",
            "debug",
            "halt",
            "halt(1)",
            "halt_error",
            "def f: 1 + f; f",
            "[.[] | def f: .; f]",
            "\"\\(def f: .; f)\"",
            "include \"secret\"; .",
        ] {
            assert!(JqProgram::parse_field(query).is_err(), "{query}");
        }
    }
}
