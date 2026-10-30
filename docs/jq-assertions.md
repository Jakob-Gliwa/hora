# jq assertions

HTTP and MQTT monitors accept `jq`, `jq_expected` (default `"true"`) and
`jq_operator` (`==`, `!=`, `<`, `<=`, `>`, `>=`). Expressions use the embedded
pure Rust jaq interpreter: no executable is launched per check. They compile
at config load and compiled code is shared across config snapshots.

```toml
jq = '.data.value | tonumber'
jq_expected = "10"
jq_operator = ">="
```

Exactly one non-null scalar must be produced. Missing output, null, malformed
JSON, runtime errors and multiple outputs fail the check. A filter may handle
missing data explicitly, for example `if .value == null then 0 else .value end`.
Ordered comparisons require finite numbers; equality also accepts numeric
strings. Existing JSONPath assertions remain supported unchanged.

Only pure base functions are enabled. Environment access, logging, imports
and file I/O are unavailable. Expressions are trusted operator configuration;
use finite filters on the capped payload (avoid unbounded recursion).

The migration tool translates a documented JSONata subset into jq and rejects
unsupported expressions instead of weakening an assertion.
