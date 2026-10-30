use super::env::expand_env;
use super::raw::default_timeout;
use super::*;

fn parse(toml_src: &str) -> Config {
    toml::from_str(toml_src).expect("valid config")
}

/// The whole loader (deserialization, which validates each monitor, then the
/// cross-checks), as `hora check` runs it.
fn load(toml_src: &str) -> anyhow::Result<Config> {
    super::parse(toml_src)
}

fn http_spec(monitor: &Monitor) -> &HttpSpec {
    match &monitor.spec {
        MonitorKind::Http(spec) => spec,
        other => panic!("not an http monitor: {other:?}"),
    }
}

fn dns_spec(monitor: &Monitor) -> &DnsSpec {
    match &monitor.spec {
        MonitorKind::Dns(spec) => spec,
        other => panic!("not a dns monitor: {other:?}"),
    }
}

#[test]
fn ad_hoc_monitor_carries_only_kind_and_target() {
    let monitor = Monitor::ad_hoc(Kind::Tcp, "db.example.com:5432".to_owned()).unwrap();
    assert_eq!(monitor.kind(), Kind::Tcp);
    assert_eq!(monitor.target(), "db.example.com:5432");
    // Defaults the probe path relies on.
    assert_eq!(monitor.timeout_secs, default_timeout());
    // No anti-flap retry for a one-shot: the honest first result.
    assert_eq!(monitor.probe_retries(), 0);
    assert!(!monitor.dual_stack());
    assert!(monitor.confirm_with_peers.is_none());
    // An https ad-hoc monitor opts into the cert check like a real one.
    let https = Monitor::ad_hoc(Kind::Http, "https://example.com".to_owned()).unwrap();
    assert!(https.checks_cert());
}

const MINIMAL: &str = r#"
        [page]
        [server]
        [[monitors]]
        id = "web"
        name = "Web"
        target = "https://example.com"
        interval_secs = 60
    "#;

#[test]
fn applies_defaults() {
    let config = parse(MINIMAL);
    assert_eq!(config.page.history_days, 90);
    assert_eq!(config.server.bind, "127.0.0.1:8787");
    assert_eq!(config.alerts.fail_threshold, 3);
    assert_eq!(config.alerts.cert_expiry_days, 14);
    assert_eq!(config.alerts.default_retention_days, 90);

    let monitor = &config.monitors[0];
    assert_eq!(monitor.kind(), Kind::Http);
    assert_eq!(monitor.timeout_secs, 10);
    assert_eq!(monitor.retention_days(90), 90);
}

#[test]
fn cert_checking_auto_detects_https() {
    let config = parse(MINIMAL);
    assert!(config.monitors[0].checks_cert());

    let plain = parse(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "tcp"
            name = "DB"
            kind = "tcp"
            target = "db:5432"
            interval_secs = 60
        "#,
    );
    assert!(!plain.monitors[0].checks_cert());
}

#[test]
fn respects_overrides() {
    let config = parse(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "web"
            name = "Web"
            target = "https://example.com"
            interval_secs = 60
            check_cert = false
            retention_days = 7
        "#,
    );
    assert!(!config.monitors[0].checks_cert());
    assert_eq!(config.monitors[0].retention_days(90), 7);
}

#[test]
fn parses_custom_headers() {
    let config = parse(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "x"
            name = "X"
            target = "https://crates.io"
            interval_secs = 60
            headers = { Accept = "text/html", "X-Token" = "abc" }
        "#,
    );
    let headers = &http_spec(&config.monitors[0]).headers;
    assert_eq!(headers.get("Accept").map(AsRef::as_ref), Some("text/html"));
    assert_eq!(headers.get("X-Token").map(AsRef::as_ref), Some("abc"));
}

#[test]
fn rejects_duplicate_ids() {
    let error = load(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "dup"
            name = "One"
            target = "https://a.example"
            interval_secs = 60
            [[monitors]]
            id = "dup"
            name = "Two"
            target = "https://b.example"
            interval_secs = 60
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("duplicate monitor id"), "got: {error}");
}

#[test]
fn rejects_zero_timeout() {
    let error = load(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "x"
            name = "X"
            target = "https://example.com"
            interval_secs = 60
            timeout_secs = 0
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("timeout_secs"), "got: {error}");
}

#[test]
fn channel_debug_redacts_secrets() {
    let telegram = Channel::Telegram {
        name: "ops".to_owned(),
        token: Secret("supersecret".to_owned()),
        chat_id: "42".to_owned(),
    };
    let shown = format!("{telegram:?}");
    assert!(!shown.contains("supersecret"), "token leaked: {shown}");
    assert!(shown.contains("<redacted>"));
    assert!(shown.contains("ops") && shown.contains("42"));

    let discord = Channel::Discord {
        name: "web".to_owned(),
        webhook_url: Secret("https://discord.com/api/webhooks/123/supersecret".to_owned()),
    };
    let shown = format!("{discord:?}");
    assert!(!shown.contains("supersecret"), "webhook leaked: {shown}");
    assert!(shown.contains("<redacted>"));
}

#[test]
fn parses_http_assertions() {
    let config = parse(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "api"
            name = "API"
            target = "https://example.com/health"
            interval_secs = 60
            keyword = "operational"
            json_query = "$.status"
            json_expected = "ok"
            number_regex = 'ships">(\d+)<'
            number_min = 1
            number_max = 500
        "#,
    );
    let monitor = http_spec(&config.monitors[0]);
    assert_eq!(
        monitor
            .keyword
            .as_ref()
            .map(|keyword| keyword.text.as_str()),
        Some("operational")
    );
    let json = monitor.json.as_ref().expect("json assertion");
    assert_eq!(json.query.raw(), "$.status");
    assert_eq!(json.expected.as_deref(), Some("ok"));
    let number = monitor.number.as_ref().expect("number assertion");
    assert_eq!(number.regex.raw(), r#"ships">(\d+)<"#);
    assert_eq!(number.min, Some(1));
    assert_eq!(number.max, Some(500));
    validate(&config).expect("valid assertions");
}

#[test]
fn rejects_invalid_number_regex() {
    let error = load(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "api"
            name = "API"
            target = "https://example.com"
            interval_secs = 60
            number_regex = "ships (["
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("invalid number_regex"), "got: {error}");
}

#[test]
fn rejects_number_bounds_without_regex() {
    let error = load(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "api"
            name = "API"
            target = "https://example.com"
            interval_secs = 60
            number_min = 1
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("require number_regex"), "got: {error}");
}

#[test]
fn rejects_inverted_number_bounds() {
    let error = load(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "api"
            name = "API"
            target = "https://example.com"
            interval_secs = 60
            number_regex = '(\d+)'
            number_min = 10
            number_max = 5
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("exceeds number_max"), "got: {error}");
}

#[test]
fn rejects_assertions_on_tcp() {
    let error = load(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "db"
            name = "DB"
            kind = "tcp"
            target = "db:5432"
            interval_secs = 60
            keyword = "nope"
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("require an http monitor"), "got: {error}");
}

#[test]
fn rejects_invalid_json_query() {
    let error = load(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "api"
            name = "API"
            target = "https://example.com"
            interval_secs = 60
            json_query = "not a path"
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("invalid json_query"), "got: {error}");
}

#[test]
fn parses_push_monitor_without_target() {
    let config = parse(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "cron"
            name = "Nightly backup"
            kind = "push"
            interval_secs = 90000
            push_token = "secret"
        "#,
    );
    assert_eq!(config.monitors[0].kind(), Kind::Push);
    assert_eq!(config.monitors[0].target(), "");
    assert_eq!(
        config.monitors[0].push_token.as_ref().map(AsRef::as_ref),
        Some("secret")
    );
    validate(&config).expect("push monitor valid without target");
}

#[test]
fn rejects_empty_target_on_http() {
    let error = load(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "x"
            name = "X"
            interval_secs = 60
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("target must not be empty"), "got: {error}");
}

#[test]
fn parses_and_validates_proxy() {
    let config = parse(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "via"
            name = "Via"
            target = "https://example.com"
            interval_secs = 60
            proxy = "socks5://127.0.0.1:9050"
        "#,
    );
    assert_eq!(config.monitors[0].proxy(), Some("socks5://127.0.0.1:9050"));
    validate(&config).expect("valid proxy");
}

#[test]
fn rejects_proxy_on_tcp() {
    let error = load(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "db"
            name = "DB"
            kind = "tcp"
            target = "db:5432"
            interval_secs = 60
            proxy = "http://127.0.0.1:8080"
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("require an http monitor"), "got: {error}");
}

#[test]
fn dual_stack_validation() {
    let base = |body: &str| {
        format!(
            r#"
                [page]
                [server]
                [[monitors]]
                id = "x"
                name = "X"
                interval_secs = 60
                dual_stack = true
                {body}
            "#
        )
    };

    // Hostname targets pass for the three active kinds.
    for body in [
        "target = \"https://example.com\"",
        "kind = \"tcp\"\ntarget = \"db.example.com:5432\"",
        "kind = \"icmp\"\ntarget = \"gw.example.com\"",
    ] {
        load(&base(body)).expect("hostname target valid");
    }

    // An IP literal has a single family - rejected, brackets included.
    for body in [
        "target = \"https://192.0.2.1/health\"",
        "target = \"https://[2001:db8::1]/health\"",
        "kind = \"tcp\"\ntarget = \"192.0.2.1:5432\"",
        "kind = \"icmp\"\ntarget = \"2001:db8::1\"",
    ] {
        let error = load(&base(body)).unwrap_err().to_string();
        assert!(error.contains("requires a hostname"), "got: {error}");
    }

    // Wrong kind, and proxy combination.
    let error = load(&base("kind = \"dns\"\ntarget = \"example.com\""))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("requires an http, tcp or icmp monitor"),
        "got: {error}"
    );
    let error = load(&base(
        "target = \"https://example.com\"\nproxy = \"http://127.0.0.1:8080\"",
    ))
    .unwrap_err()
    .to_string();
    assert!(error.contains("cannot go through a proxy"), "got: {error}");
}

#[test]
fn maintenance_window_mutes_selected_monitor() {
    let config = parse(
        r#"
            [page]
            [server]
            [[maintenance]]
            title = "DB upgrade"
            start = "2026-06-08T00:00:00Z"
            end = "2026-06-08T02:00:00Z"
            monitors = ["db"]
            [[monitors]]
            id = "db"
            name = "DB"
            kind = "tcp"
            target = "db:5432"
            interval_secs = 60
        "#,
    );
    let parse_dt = |s: &str| s.parse::<chrono::DateTime<chrono::Utc>>().unwrap();
    assert!(config.in_maintenance("db", parse_dt("2026-06-08T01:00:00Z")));
    // Outside the window, or a monitor not listed.
    assert!(!config.in_maintenance("db", parse_dt("2026-06-08T03:00:00Z")));
    assert!(!config.in_maintenance("web", parse_dt("2026-06-08T01:00:00Z")));
    validate(&config).expect("valid maintenance");
}

#[test]
fn rejects_inverted_maintenance_window() {
    let error = load(
        r#"
            [page]
            [server]
            [[maintenance]]
            start = "2026-06-08T02:00:00Z"
            end = "2026-06-08T00:00:00Z"
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("start must be before end"), "got: {error}");
}

#[test]
fn parses_incidents() {
    let config = parse(
        r#"
            [page]
            [server]
            [[incidents]]
            title = "Investigating elevated latency"
            body = "We are looking into it."
            severity = "warning"
            at = "2026-06-07T12:00:00Z"
        "#,
    );
    assert_eq!(config.incidents.len(), 1);
    assert_eq!(config.incidents[0].severity, Severity::Warning);
    assert!(config.incidents[0].at.is_some());
}

#[test]
fn rejects_non_url_safe_id() {
    let error = load(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "a/b"
            name = "X"
            target = "https://example.com"
            interval_secs = 60
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("alphanumeric"), "got: {error}");
}

#[test]
fn secret_is_redacted_in_debug() {
    assert_eq!(
        format!("{:?}", Secret("supersecret".to_owned())),
        "<redacted>"
    );
    assert_eq!(format!("{:?}", Secret(String::new())), "<unset>");
}

#[test]
fn monitor_debug_redacts_url_credentials() {
    let config = parse(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "web"
            name = "Web"
            target = "https://user:s3cret@example.com/health"
            interval_secs = 60
            proxy = "socks5://puser:ppass@proxy.internal:1080"
        "#,
    );
    let dump = format!("{:?}", config.monitors[0]);
    // Credentials gone, hosts kept (so logs stay useful).
    assert!(!dump.contains("s3cret"), "target password leaked: {dump}");
    assert!(!dump.contains("ppass"), "proxy password leaked: {dump}");
    assert!(!dump.contains("puser"), "proxy username leaked: {dump}");
    assert!(dump.contains("example.com"), "target host lost: {dump}");
    assert!(dump.contains("proxy.internal"), "proxy host lost: {dump}");
}

#[test]
fn monitor_debug_redacts_query_secrets() {
    let mut monitor = Monitor::ad_hoc(
        Kind::Http,
        "https://api.example.com/health?api_key=s3cret&verbose=1".to_owned(),
    )
    .unwrap();
    // The ad-hoc name is the target; a configured one is not.
    monitor.name = "API".to_owned();
    let dump = format!("{monitor:?}");
    assert!(!dump.contains("s3cret"), "query secret leaked: {dump}");
    // Keys stay, so the log still says *what* was sent.
    assert!(dump.contains("api_key=***&verbose=***"), "{dump}");
    // Strings that are not URLs (a tcp host:port) are untouched.
    let tcp = Monitor::ad_hoc(Kind::Tcp, "db.example.com:5432".to_owned()).unwrap();
    assert!(format!("{tcp:?}").contains("db.example.com:5432"));

    // The release watch's version endpoint is redacted the same way.
    monitor.release = Some(ReleaseWatch {
        github: "a/b".to_owned(),
        current: None,
        current_url: Some("https://x.example/version?token=s3cret".to_owned()),
        current_query: Some(Parsed::new("$.version")),
    });
    let dump = format!("{monitor:?}");
    assert!(!dump.contains("s3cret"), "query secret leaked: {dump}");
    assert!(
        dump.contains("token=***") && dump.contains("$.version"),
        "{dump}"
    );
}

#[test]
fn channel_debug_shows_routing_fields() {
    // The derived Debug keeps the non-secret fields readable and every
    // credential redacted, for each variant shape.
    let email = Channel::Email {
        name: "mail".to_owned(),
        host: "smtp.example.com".to_owned(),
        port: 587,
        username: "bot".to_owned(),
        password: Secret("pa55word".to_owned()),
        from: "a@example.com".to_owned(),
        to: "b@example.com".to_owned(),
        implicit_tls: false,
    };
    let ntfy = Channel::Ntfy {
        name: "push".to_owned(),
        url: Secret("https://ntfy.sh/s3cret-topic".to_owned()),
        token: Some(Secret("tk_s3cret".to_owned())),
    };
    let dump = format!("{email:?} {ntfy:?}");
    assert!(
        !dump.contains("pa55word") && !dump.contains("s3cret"),
        "{dump}"
    );
    assert!(dump.contains("smtp.example.com") && dump.contains("b@example.com"));
}

#[test]
fn expand_env_dollar_escape() {
    // `$$` is a literal `$`, so `$${id}` is a literal `${id}` (no env lookup).
    assert_eq!(expand_env("$${id}"), "${id}");
    assert_eq!(expand_env("a$$b"), "a$b");
}

#[test]
fn env_expansion_only_touches_string_values() {
    // Expansion runs on the parsed document, so a `${VAR}` inside a comment
    // is never looked up or hydrated (raw-text expansion used to splice
    // values - and unset-variable warnings - into commented-out examples).
    let toml = r#"
            [page]
            [server]
            # auth_token = "${HORA_SURELY_UNSET_COMMENTED_EXAMPLE}"
            [[monitors]]
            id = "x"
            name = "$${literal}"
            target = "https://example.com"
            interval_secs = 60
        "#;
    let config = super::parse(toml).expect("a commented ${VAR} must not affect parsing");
    // `$$` in a real string value still unescapes to a literal `$`.
    assert_eq!(config.monitors[0].name, "${literal}");
}

#[test]
fn rejects_negative_latency_threshold() {
    let error = load(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "x"
            name = "X"
            target = "https://example.com"
            interval_secs = 60
            degraded_over_ms = -1
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("degraded_over_ms must be > 0"),
        "got: {error}"
    );
}

#[test]
fn rejects_malformed_http_target() {
    let error = load(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "x"
            name = "X"
            target = "https//example.com"
            interval_secs = 60
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("http(s) URL"), "got: {error}");
}

#[test]
fn parses_named_channels_and_routing() {
    let config = parse(
        r#"
            [page]
            [server]
            client_ip_header = "cf-connecting-ip"

            [[channels]]
            name = "ops"
            type = "telegram"
            token = "t"
            chat_id = "42"

            [[channels]]
            name = "web-discord"
            type = "discord"
            webhook_url = "https://discord.com/api/webhooks/1/a"

            [[channels]]
            name = "alerts-discord"
            type = "discord"
            webhook_url = "https://discord.com/api/webhooks/2/b"

            [[monitors]]
            id = "web"
            name = "Web"
            target = "https://example.com"
            interval_secs = 60
            notify = ["web-discord", "ops"]
        "#,
    );
    assert_eq!(
        config.server.client_ip_header.as_deref(),
        Some("cf-connecting-ip")
    );
    // Two channels share the discord type, routed independently by name.
    assert_eq!(config.channels.len(), 3);
    assert_eq!(config.channels[1].name(), "web-discord");
    assert_eq!(config.channels[2].name(), "alerts-discord");
    let notify = config.monitors[0].notify.as_ref().expect("notify set");
    assert_eq!(notify, &["web-discord", "ops"]);
    validate(&config).expect("valid config");
}

#[test]
fn parses_matrix_and_freemobile_channels() {
    let config = parse(
        r#"
            [page]
            [server]

            [[channels]]
            name = "ops-matrix"
            type = "matrix"
            homeserver = "https://matrix.example.org"
            token = "syt_secret"
            room_id = "!room:matrix.example.org"

            [[channels]]
            name = "oncall-sms"
            type = "freemobile"
            user = "12345678"
            pass = "apikey"
        "#,
    );
    assert_eq!(config.channels.len(), 2);
    assert_eq!(config.channels[0].name(), "ops-matrix");
    assert_eq!(config.channels[1].name(), "oncall-sms");
    assert!(config.channels[0].is_configured() && config.channels[1].is_configured());
    validate(&config).expect("valid");
    // Neither secret may surface through Debug.
    let dump = format!("{:?}", config.channels);
    assert!(!dump.contains("syt_secret"), "matrix token leaked: {dump}");
    assert!(!dump.contains("apikey"), "freemobile pass leaked: {dump}");
}

#[test]
fn alert_on_degraded_defaults_off_and_parses() {
    let off = parse("[page]\n[server]\n");
    assert!(!off.alerts.alert_on_degraded);

    let on = parse("[page]\n[server]\n[alerts]\nalert_on_degraded = true\n");
    assert!(on.alerts.alert_on_degraded);
}

#[test]
fn parses_email_channel_with_default_port() {
    let config = parse(
        r#"
            [page]
            [server]
            [[channels]]
            name = "mail"
            type = "email"
            host = "smtp.example.com"
            username = "u"
            password = "${SMTP_PASSWORD}"
            from = "Hora <hora@example.com>"
            to = "ops@example.com"
        "#,
    );
    match &config.channels[0] {
        Channel::Email {
            host, port, from, ..
        } => {
            assert_eq!(host, "smtp.example.com");
            assert_eq!(*port, 587);
            assert_eq!(from, "Hora <hora@example.com>");
        }
        other => panic!("expected email channel, got {other:?}"),
    }
    validate(&config).expect("valid email channel");
}

#[test]
fn rejects_duplicate_channel_names() {
    let error = load(
        r#"
            [page]
            [server]
            [[channels]]
            name = "dup"
            type = "discord"
            webhook_url = "https://x/1"
            [[channels]]
            name = "dup"
            type = "slack"
            webhook_url = "https://y/2"
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("duplicate channel name"), "got: {error}");
}

#[test]
fn exec_monitors_are_gated_and_confined() {
    let base = r#"
            [page]
            [server]
            [[monitors]]
            id = "raid"
            name = "RAID"
            kind = "exec"
            {body}
            interval_secs = 300
        "#;
    let dir = Some(std::env::temp_dir());

    // No HORA_EXEC_DIR: rejected however valid the rest is.
    let toml = base.replace("{body}", r#"command = ["check_raid"]"#);
    let err = super::parse_with_exec_dir(&toml, None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("HORA_EXEC_DIR"), "{err}");

    // A directory that does not exist: rejected at load.
    let err = super::parse_with_exec_dir(
        &toml,
        Some(std::path::PathBuf::from("/nonexistent-hora-exec")),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("not a directory"), "{err}");

    // With the gate set, the same config is valid.
    super::parse_with_exec_dir(&toml, dir.clone()).expect("valid exec monitor");

    // An empty command, a path in command[0], or a target: all rejected.
    for (body, expect) in [
        ("command = []", "non-empty"),
        (r#"command = ["../sh"]"#, "bare file name"),
        (r#"command = ["sub/dir"]"#, "bare file name"),
        (
            "command = [\"x\"]\n            target = \"https://e.com\"",
            "not a `target`",
        ),
    ] {
        let toml = base.replace("{body}", body);
        let err = super::parse_with_exec_dir(&toml, dir.clone())
            .unwrap_err()
            .to_string();
        assert!(err.contains(expect), "{body}: {err}");
    }

    // `command` on a non-exec monitor is a mistake, not decoration.
    let err = super::parse_with_exec_dir(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "web"
            name = "Web"
            target = "https://example.com"
            command = ["check_http"]
            interval_secs = 60
        "#,
        dir,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("requires kind"), "{err}");
}

#[test]
fn confirm_with_peers_requires_health_and_a_probeable_peer() {
    // Global flag without [health]: rejected.
    let err = super::parse(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "web"
            name = "Web"
            target = "https://example.com"
            interval_secs = 60
            confirm_with_peers = true
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("[health]"), "{err}");

    // [health] flag but no peer with a ping_url: rejected.
    let err = super::parse(
        r#"
            [page]
            [server]
            [health]
            id = "hora-a"
            confirm_with_peers = true
            [[peers]]
            id = "hora-b"
            name = "B"
            expect_every_secs = 60
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("ping_url"), "{err}");

    // A push monitor cannot opt in (nothing to probe).
    let err = super::parse(
        r#"
            [page]
            [server]
            [health]
            id = "hora-a"
            [[peers]]
            id = "hora-b"
            name = "B"
            ping_url = "https://b.example/api/push/hora-a"
            [[monitors]]
            id = "beat"
            name = "Beat"
            kind = "push"
            interval_secs = 60
            confirm_with_peers = true
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("push"), "{err}");

    // The healthy shape parses, and probe_url derives from ping_url's origin.
    let config = super::parse(
        r#"
            [page]
            [server]
            [health]
            id = "hora-a"
            confirm_with_peers = true
            [[peers]]
            id = "hora-b"
            name = "B"
            ping_url = "https://b.example:8443/api/push/hora-a"
            [[monitors]]
            id = "web"
            name = "Web"
            target = "https://example.com"
            interval_secs = 60
        "#,
    )
    .expect("valid");
    assert_eq!(
        config.peers[0].probe_url().as_deref(),
        Some("https://b.example:8443/api/peer/probe")
    );
}

#[test]
fn cron_keeps_the_3x_shorthand_steps() {
    // croner 4 rejects these by default; configs written for earlier
    // releases must keep loading.
    for pattern in ["5/5 * * * *", "/10 * * * *", "0 8 * * 1", "*/15 * * * *"] {
        assert!(super::parse_cron(pattern).is_ok(), "{pattern}");
    }
    assert!(super::parse_cron("61 * * * *").is_err());
    let from = chrono::DateTime::from_timestamp(0, 0).expect("epoch");
    let next = super::parse_cron("5/5 * * * *")
        .expect("valid")
        .find_next_occurrence(&from, false)
        .expect("next");
    assert_eq!(next.timestamp(), 300);
}

#[test]
fn digest_validates_schedule_and_routes() {
    let base = r#"
            [page]
            [server]
            [[channels]]
            name = "ops"
            type = "slack"
            webhook_url = "https://x/1"
            [digest]
            {digest}
        "#;

    // The default schedule is Monday 08:00 UTC; routes must exist.
    let config = super::parse(&base.replace("{digest}", "")).expect("valid digest");
    assert_eq!(config.digest.as_ref().unwrap().schedule, "0 8 * * 1");

    let err = super::parse(&base.replace("{digest}", "schedule = \"not cron\""))
        .unwrap_err()
        .to_string();
    assert!(err.contains("invalid schedule"), "{err}");

    let err = super::parse(&base.replace("{digest}", "notify = [\"typo\"]"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("unknown channel"), "{err}");
}

#[test]
fn rejects_notify_unknown_channel() {
    let error = load(
        r#"
            [page]
            [server]
            [[channels]]
            name = "ops"
            type = "slack"
            webhook_url = "https://x/1"
            [[monitors]]
            id = "web"
            name = "Web"
            target = "https://example.com"
            interval_secs = 60
            notify = ["typo"]
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("unknown channel"), "got: {error}");
}

#[test]
fn notify_unconfirmed_names_real_channels() {
    let config = |routes: &str| {
        format!(
            r#"
            [page]
            [server]
            [alerts]
            notify_unconfirmed = {routes}
            [[channels]]
            name = "ops"
            type = "slack"
            webhook_url = "https://x/1"
            "#
        )
    };
    let loaded = load(&config(r#"["ops"]"#)).expect("valid");
    assert_eq!(
        loaded.alerts.notify_unconfirmed.as_deref(),
        Some(["ops".to_owned()].as_slice())
    );
    let error = load(&config(r#"["typo"]"#)).unwrap_err().to_string();
    assert!(
        error.contains("alerts.notify_unconfirmed: notify references unknown channel"),
        "got: {error}"
    );
}

#[test]
fn parses_health_and_peers() {
    let config = parse(
        r#"
            [page]
            [server]
            [health]
            id = "hora-a"
            quorum = true
            [[peers]]
            id = "hora-b"
            name = "Hora B"
            ping_url = "https://b.example/api/push/hora-a"
            ping_token = "tok"
            listen_token = "in"
            expect_every_secs = 90
        "#,
    );
    let health = config.health.as_ref().expect("health");
    assert_eq!(health.id, "hora-a");
    assert_eq!(health.interval_secs, 60); // default
    assert!(health.quorum);
    let peer = &config.peers[0];
    assert!(peer.is_watched());
    assert_eq!(peer.listen_id(), "hora-b"); // defaults to id
    assert_eq!(
        peer.effective_witness_url().as_deref(),
        Some("https://b.example/healthz")
    );
    validate(&config).expect("valid health + peers");
}

#[test]
fn rejects_peers_without_health() {
    let error = load(
        r#"
            [page]
            [server]
            [[peers]]
            id = "hora-b"
            name = "Hora B"
            ping_url = "https://b.example/api/push/hora-a"
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("require a [health] section"), "got: {error}");
}

#[test]
fn rejects_peer_that_does_nothing() {
    let error = load(
        r#"
            [page]
            [server]
            [health]
            id = "hora-a"
            [[peers]]
            id = "hora-b"
            name = "Hora B"
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("ping_url") && error.contains("expect_every_secs"),
        "got: {error}"
    );
}

#[test]
fn rejects_peer_listen_id_clashing_with_monitor() {
    let error = load(
        r#"
            [page]
            [server]
            [health]
            id = "hora-a"
            [[monitors]]
            id = "shared"
            name = "Web"
            target = "https://example.com"
            interval_secs = 60
            [[peers]]
            id = "hora-b"
            name = "Hora B"
            listen_id = "shared"
            expect_every_secs = 90
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("clashes with a monitor id"), "got: {error}");
}

#[test]
fn out_only_peer_is_valid_without_watch() {
    let config = parse(
        r#"
            [page]
            [server]
            [health]
            id = "hora-a"
            [[peers]]
            id = "hc"
            name = "healthchecks.io"
            ping_url = "https://hc-ping.com/abc"
        "#,
    );
    assert!(!config.peers[0].is_watched());
    validate(&config).expect("OUT-only peer valid");
}

#[test]
fn explicit_witness_url_overrides_derivation() {
    let config = parse(
        r#"
            [page]
            [server]
            [health]
            id = "hora-a"
            [[peers]]
            id = "hora-b"
            name = "Hora B"
            ping_url = "https://b.example:9000/api/push/hora-a"
            witness_url = "https://b.internal/healthz"
            expect_every_secs = 90
        "#,
    );
    assert_eq!(
        config.peers[0].effective_witness_url().as_deref(),
        Some("https://b.internal/healthz")
    );
}

#[test]
fn peer_debug_redacts_secrets() {
    let config = parse(
        r#"
            [page]
            [server]
            [health]
            id = "hora-a"
            heartbeat_url = "https://hc-ping.com/sup3rsecret"
            [[peers]]
            id = "hora-b"
            name = "Hora B"
            ping_url = "https://b.example/api/push/hora-a?tok=sup3rsecret"
            ping_token = "tok3n"
            expect_every_secs = 90
            witness_url = "https://b.example/healthz?key=sup3rsecret"
        "#,
    );
    let dump = format!("{:?}", config.health) + &format!("{:?}", config.peers);
    assert!(!dump.contains("sup3rsecret"), "url secret leaked: {dump}");
    assert!(!dump.contains("tok3n"), "ping token leaked: {dump}");
}

#[test]
fn parses_group_and_depends_on() {
    let config = parse(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "db"
            name = "DB"
            kind = "tcp"
            target = "db:5432"
            interval_secs = 30
            group = "infra"
            [[monitors]]
            id = "api"
            name = "API"
            target = "https://api.example"
            interval_secs = 30
            group = "app"
            depends_on = ["db"]
        "#,
    );
    assert_eq!(config.monitors[0].group.as_deref(), Some("infra"));
    assert_eq!(
        config.monitors[1].depends_on.as_deref(),
        Some(&vec!["db".to_owned()][..])
    );
    validate(&config).expect("valid topology");
}

#[test]
fn rejects_depends_on_unknown_monitor() {
    let error = load(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "api"
            name = "API"
            target = "https://api.example"
            interval_secs = 30
            depends_on = ["ghost"]
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("unknown monitor"), "got: {error}");
}

#[test]
fn rejects_dependency_cycle() {
    let error = load(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "a"
            name = "A"
            target = "https://a.example"
            interval_secs = 30
            depends_on = ["b"]
            [[monitors]]
            id = "b"
            name = "B"
            target = "https://b.example"
            interval_secs = 30
            depends_on = ["a"]
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("cycle"), "got: {error}");
}

#[test]
fn parses_icmp_monitor() {
    let config = parse(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "host"
            name = "Host"
            kind = "icmp"
            target = "192.0.2.1"
            interval_secs = 30
        "#,
    );
    assert_eq!(config.monitors[0].kind(), Kind::Icmp);
    // ICMP is not HTTPS, so no certificate check.
    assert!(!config.monitors[0].checks_cert());
    validate(&config).expect("valid icmp monitor");
}

#[test]
fn rejects_icmp_target_with_scheme_or_port() {
    for bad in ["https://example.com", "1.2.3.4:443", "a/b"] {
        let error = load(&format!(
            r#"
                [page]
                [server]
                [[monitors]]
                id = "host"
                name = "Host"
                kind = "icmp"
                target = "{bad}"
                interval_secs = 30
            "#
        ))
        .unwrap_err()
        .to_string();
        assert!(error.contains("bare host or IP"), "{bad} -> {error}");
    }
}

#[test]
fn accepts_dns_monitor() {
    let config = parse(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "dns"
            name = "DNS"
            kind = "dns"
            target = "example.com"
            interval_secs = 300
            dns_record = "A"
            dns_expected = "1.2.3.4, 5.6.7.8"
            dns_resolver = "9.9.9.9:53"
        "#,
    );
    validate(&config).expect("valid dns monitor");
    let monitor = dns_spec(&config.monitors[0]);
    assert_eq!(monitor.resolver, Some("9.9.9.9:53".parse().unwrap()));
    assert_eq!(monitor.record, DnsRecord::A);
}

#[test]
fn dns_fields_parse_once_at_load() {
    // Bracketed IPv6 resolvers (what `hora import kuma` emits) and
    // lowercase record types are accepted and typed.
    let config = parse(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "dns"
            name = "DNS"
            kind = "dns"
            target = "example.com"
            interval_secs = 300
            dns_record = "aaaa"
            dns_resolver = "[2620:fe::fe]:53"
        "#,
    );
    validate(&config).expect("valid dns monitor");
    let monitor = dns_spec(&config.monitors[0]);
    assert_eq!(monitor.resolver, Some("[2620:fe::fe]:53".parse().unwrap()));
    assert_eq!(monitor.record, DnsRecord::Aaaa);
}

#[test]
fn host_port_parsing_is_bracket_aware() {
    assert_eq!(split_host_port("db:5432"), Some(("db", 5432)));
    assert_eq!(
        split_host_port("[2001:db8::1]:443"),
        Some(("2001:db8::1", 443))
    );
    // Unbracketed IPv6 + port is ambiguous; missing or bad parts fail.
    assert_eq!(split_host_port("::1:80"), None);
    assert_eq!(split_host_port(":80"), None);
    assert_eq!(split_host_port("db"), None);
    assert_eq!(split_host_port("db:http"), None);
    assert_eq!(split_host_port("[not-v6]:80"), None);
    assert_eq!(split_host_port("[::1]"), None);

    let tcp = |target: &str| Monitor::ad_hoc(Kind::Tcp, target.to_owned());
    assert!(tcp("[::1]:80").is_ok());
    let error = tcp("::1:80").unwrap_err().to_string();
    assert!(error.contains("must be host:port"), "{error}");
}

#[test]
fn periods_and_status_are_bounded() {
    let check = |field: &str| {
        load(&format!(
            r#"
                [page]
                [server]
                [[monitors]]
                id = "web"
                name = "Web"
                target = "https://example.com"
                {field}
            "#
        ))
        .map_err(|err| err.to_string())
    };
    // A typo'd huge interval fails at load instead of panicking a task.
    let error = check("interval_secs = 86400000000000").unwrap_err();
    assert!(error.contains("at most"), "{error}");
    let error = check("interval_secs = 60\ntimeout_secs = 86400000000000").unwrap_err();
    assert!(error.contains("timeout_secs must be at most"), "{error}");
    assert!(check("interval_secs = 2592000").is_ok());
    // Out-of-range expected statuses could never match a response.
    for status in [0, 99, 600, 1000] {
        let error = check(&format!("interval_secs = 60\nexpected_status = {status}")).unwrap_err();
        assert!(error.contains("not an HTTP status"), "{error}");
    }
    assert!(check("interval_secs = 60\nexpected_status = 301").is_ok());

    let peer = |every: u64| {
        let config = parse(&format!(
            r#"
                [page]
                [server]
                [health]
                id = "a"
                [[peers]]
                id = "b"
                name = "B"
                expect_every_secs = {every}
                listen_token = "a-long-enough-listen-token"
            "#
        ));
        validate(&config).map_err(|err| err.to_string())
    };
    assert!(peer(90).is_ok());
    let error = peer(86_400_000_000).unwrap_err();
    assert!(
        error.contains("expect_every_secs must be at most"),
        "{error}"
    );
}

#[test]
fn exec_dir_is_canonicalized_at_load() {
    let config = parse_with_exec_dir(MINIMAL, Some(std::env::temp_dir())).expect("config");
    assert_eq!(
        config.exec_dir,
        Some(std::env::temp_dir().canonicalize().unwrap())
    );
}

#[test]
fn rejects_invalid_dns_monitor() {
    for (field, message) in [
        ("dns_record = \"WHATEVER\"", "unsupported dns_record"),
        ("dns_resolver = \"no-port\"", "must be host:port"),
        ("dns_resolver = \"host:notaport\"", "must be host:port"),
        // Unbracketed IPv6 is ambiguous; a hostname can't be the resolver.
        ("dns_resolver = \"2620:fe::fe:53\"", "must be host:port"),
        ("dns_resolver = \"dns.google:53\"", "not a hostname"),
    ] {
        let error = load(&format!(
            r#"
                [page]
                [server]
                [[monitors]]
                id = "dns"
                name = "DNS"
                kind = "dns"
                target = "example.com"
                interval_secs = 300
                {field}
            "#
        ))
        .unwrap_err()
        .to_string();
        assert!(error.contains(message), "{field} -> {error}");
    }
}

#[test]
fn rejects_dns_target_with_scheme() {
    let error = load(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "dns"
            name = "DNS"
            kind = "dns"
            target = "https://example.com"
            interval_secs = 300
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("must be a hostname"), "{error}");
}

#[test]
fn rejects_dns_fields_on_http() {
    let error = load(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "web"
            name = "Web"
            target = "https://example.com"
            interval_secs = 60
            dns_record = "A"
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("require a dns monitor"), "{error}");
}

#[test]
fn private_monitor_requires_auth_token() {
    let private = r#"
            [page]
            [server]
            {token}
            [[monitors]]
            id = "internal"
            name = "Internal"
            target = "https://internal.example"
            interval_secs = 60
            public = false
        "#;

    let error = load(&private.replace("{token}", ""))
        .unwrap_err()
        .to_string();
    assert!(error.contains("requires server.auth_token"), "{error}");

    let config = parse(&private.replace("{token}", "auth_token = \"s3cret\""));
    validate(&config).expect("private monitor with token is valid");

    // An interpolated-but-unset token expands to "" - now rejected outright
    // (an empty token would authorize a blank `?token=`), not silently treated
    // as "no token".
    let error = load(&private.replace("{token}", "auth_token = \"\""))
        .unwrap_err()
        .to_string();
    assert!(error.contains("auth_token must not be empty"), "{error}");
}

#[test]
fn cert_pin_must_be_hex_and_is_lowercased() {
    let cfg = |pin: &str| {
        format!(
            r#"
                [page]
                [server]
                [[monitors]]
                id = "web"
                name = "Web"
                target = "https://example.com"
                interval_secs = 60
                cert_pin = "{pin}"
            "#
        )
    };

    // Wrong length / non-hex is rejected at load.
    let err = super::parse(&cfg("abc")).unwrap_err().to_string();
    assert!(err.contains("cert_pin must be 64 hex"), "{err}");

    // A valid pin typed in upper case is normalized to lowercase, so it
    // matches the lowercase fingerprint Hora computes from the cert.
    let config = super::parse(&cfg(&"A".repeat(64))).expect("valid pin");
    let lower = "a".repeat(64);
    assert_eq!(config.monitors[0].cert_pin.as_deref(), Some(lower.as_str()));
}

#[test]
fn starttls_is_tcp_only_and_protocol_checked() {
    let cfg = |kind_and_target: &str, mode: &str| {
        format!(
            r#"
                [page]
                [server]
                [[monitors]]
                id = "mail"
                name = "Mail"
                {kind_and_target}
                interval_secs = 60
                starttls = "{mode}"
            "#
        )
    };

    // The happy path: a tcp monitor speaking one of the known protocols.
    let config = super::parse(&cfg(
        "kind = \"tcp\"\ntarget = \"mail.example.org:587\"",
        "smtp",
    ))
    .expect("valid starttls monitor");
    assert!(config.monitors[0].checks_cert());
    // check_cert = false still turns the watcher off.
    let off = super::parse(&cfg(
        "kind = \"tcp\"\ntarget = \"mail.example.org:143\"\ncheck_cert = false",
        "imap",
    ))
    .expect("valid");
    assert!(!off.monitors[0].checks_cert());

    // Not tcp, or an unknown protocol: rejected at load.
    let err = super::parse(&cfg("target = \"https://example.com\"", "smtp"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("requires a tcp monitor"), "{err}");
    let err = super::parse(&cfg("kind = \"tcp\"\ntarget = \"x:21\"", "ftp"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("\"smtp\" or \"imap\""), "{err}");
}

#[test]
fn ehlo_name_is_smtp_only_and_rfc_shaped() {
    let cfg = |extra: &str| {
        format!(
            r#"
                [page]
                [server]
                [[monitors]]
                id = "mx"
                name = "MX"
                kind = "tcp"
                target = "mail.example.org:25"
                interval_secs = 60
                {extra}
            "#
        )
    };

    for name in [
        "status.example.org",
        "status.example.org.",
        "[192.0.2.1]",
        "[IPv6:2001:db8::1]",
    ] {
        let config = super::parse(&cfg(&format!(
            "starttls = \"smtp\"\nehlo_name = \"{name}\""
        )))
        .unwrap_or_else(|err| panic!("{name}: {err}"));
        assert_eq!(
            config.monitors[0].spec,
            MonitorKind::Tcp(TcpSpec {
                target: "mail.example.org:25".to_owned(),
                starttls: Some(Starttls::Smtp {
                    ehlo_name: Some(name.to_owned())
                }),
                dual_stack: false,
            })
        );
    }

    // A bare label is exactly what port-25 servers refuse; bad literals too.
    for name in [
        "hora",
        "",
        "a..b",
        "-a.b",
        "[hora]",
        "[2001:db8::1]",
        "[IPv6:1.2.3.4]",
    ] {
        let err = super::parse(&cfg(&format!(
            "starttls = \"smtp\"\nehlo_name = \"{name}\""
        )))
        .unwrap_err()
        .to_string();
        assert!(err.contains("fully qualified"), "{name}: {err}");
    }

    // Meaningless without an SMTP negotiation.
    for extra in [
        "ehlo_name = \"a.example\"",
        "starttls = \"imap\"\nehlo_name = \"a.example\"",
    ] {
        let err = super::parse(&cfg(extra)).unwrap_err().to_string();
        assert!(err.contains("requires starttls"), "{err}");
    }
}

#[test]
fn domain_expiry_must_be_a_bare_domain_and_is_lowercased() {
    let cfg = |domain: &str| {
        format!(
            r#"
                [page]
                [server]
                [[monitors]]
                id = "web"
                name = "Web"
                target = "https://api.example.com"
                interval_secs = 60
                domain_expiry = "{domain}"
            "#
        )
    };

    // URLs, host:port, paths and TLD-only names are rejected at load.
    for bad in [
        "https://example.com",
        "example.com/x",
        "example.com:443",
        "com",
    ] {
        let err = super::parse(&cfg(bad)).unwrap_err().to_string();
        assert!(err.contains("bare domain name"), "{bad}: {err}");
    }

    // Domain names are case-insensitive; canonicalized for RDAP.
    let config = super::parse(&cfg("Example.COM")).expect("valid domain");
    assert_eq!(
        config.monitors[0].domain_expiry.as_deref(),
        Some("example.com")
    );

    // The default warning window parallels the certificate one.
    assert_eq!(config.alerts.domain_expiry_days, 14);
}

#[test]
fn release_names_a_project_and_one_source_for_the_running_version() {
    let cfg = |release: &str| {
        format!(
            r#"
                [page]
                [server]
                [[monitors]]
                id = "chat"
                name = "Chat"
                target = "https://chat.example.com"
                interval_secs = 60
                release = {release}
            "#
        )
    };

    // Written down, or asked of the service (as text, or inside JSON).
    let config = super::parse(&cfg(
        r#"{ github = "matrix-construct/tuwunel", current = "v1.9.1" }"#,
    ))
    .expect("a literal version");
    let release = config.monitors[0].release.as_ref().expect("release");
    assert_eq!(release.github, "matrix-construct/tuwunel");
    assert_eq!(release.current.as_deref(), Some("v1.9.1"));
    super::parse(&cfg(
            r#"{ github = "element-hq/element-web", current_url = "https://chat.example.com/version" }"#,
        ))
        .expect("a version read as text");
    super::parse(&cfg(
            r#"{ github = "a/b", current_url = "https://x.example/v", current_query = "$.server.version" }"#,
        ))
        .expect("a version read inside JSON");

    for (bad, reason) in [
        (r#"{ github = "tuwunel", current = "1" }"#, "owner/repo"),
        (
            r#"{ github = "https://github.com/a/b", current = "1" }"#,
            "owner/repo",
        ),
        (r#"{ github = "a/b" }"#, "exactly one of"),
        (
            r#"{ github = "a/b", current = "1", current_url = "https://x.example" }"#,
            "exactly one of",
        ),
        (r#"{ github = "a/b", current = " " }"#, "is empty"),
        (
            r#"{ github = "a/b", current_url = "x.example/version" }"#,
            "http(s) URL",
        ),
        (
            r#"{ github = "a/b", current = "1", current_query = "$.v" }"#,
            "needs release.current_url",
        ),
        (
            r#"{ github = "a/b", current_url = "https://x.example", current_query = "$[" }"#,
            "invalid release.current_query",
        ),
    ] {
        let err = super::parse(&cfg(bad)).unwrap_err().to_string();
        assert!(err.contains(reason), "{bad}: {err}");
    }
}

#[test]
fn public_error_detail_parses_and_defaults_off() {
    let config = parse(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "a"
            name = "A"
            target = "https://example.com"
            interval_secs = 60
            [[monitors]]
            id = "b"
            name = "B"
            target = "https://example.org"
            interval_secs = 60
            public_error_detail = true
        "#,
    );
    assert!(!config.monitors[0].public_error_detail);
    assert!(config.monitors[1].public_error_detail);
}

#[test]
fn empty_access_token_is_rejected() {
    let toml = r#"
            [page]
            [server]
            [[monitors]]
            id = "beat"
            name = "Beat"
            kind = "push"
            interval_secs = 60
            push_token = ""
        "#;
    let config = parse(toml);
    let err = validate(&config).unwrap_err().to_string();
    assert!(err.contains("push_token must not be empty"), "{err}");
}

#[test]
fn accepts_scheduled_push_monitor() {
    let config = parse(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "backup"
            name = "Backup"
            kind = "push"
            interval_secs = 60
            schedule = "0 3 * * *"
            grace_secs = 1800
        "#,
    );
    validate(&config).expect("valid scheduled push monitor");
    let MonitorKind::Push(push) = &config.monitors[0].spec else {
        panic!("not a push monitor");
    };
    assert_eq!(push.schedule.as_ref().map(|s| s.grace_secs), Some(1800));
}

#[test]
fn rejects_bad_schedule_combinations() {
    for (fields, message) in [
        ("schedule = \"0 3 * * *\"", "requires a push monitor"),
        (
            "kind = \"push\"\nschedule = \"99 99 * * *\"",
            "invalid schedule",
        ),
        (
            "kind = \"push\"\ngrace_secs = 600",
            "grace_secs requires a schedule",
        ),
    ] {
        let error = load(&format!(
            r#"
                [page]
                [server]
                [[monitors]]
                id = "m"
                name = "M"
                target = "https://example.com"
                interval_secs = 60
                {fields}
            "#
        ))
        .unwrap_err()
        .to_string();
        assert!(error.contains(message), "{fields} -> {error}");
    }
}

#[test]
fn slo_uptime_parses_to_basis_points() {
    let config = parse(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "api"
            name = "API"
            target = "https://example.com"
            interval_secs = 60
            slo_uptime = 99.9
            slo_window_days = 7
        "#,
    );
    validate(&config).expect("valid slo");
    assert_eq!(config.monitors[0].slo_uptime, Some(9990));
    assert_eq!(config.monitors[0].slo_window_days(), 7);

    // Out-of-range targets are rejected at parse time.
    let bad = toml::from_str::<Config>(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "api"
            name = "API"
            target = "https://example.com"
            interval_secs = 60
            slo_uptime = 150.0
        "#,
    );
    assert!(bad.is_err());
}

#[test]
fn probe_retries_default_and_bounds() {
    let config = parse(MINIMAL);
    assert_eq!(config.monitors[0].probe_retries(), 1);

    let config = parse(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "raw"
            name = "Raw"
            target = "https://example.com"
            interval_secs = 60
            probe_retries = 0
        "#,
    );
    validate(&config).expect("0 disables retries");
    assert_eq!(config.monitors[0].probe_retries(), 0);

    let error = load(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "m"
            name = "M"
            target = "https://example.com"
            interval_secs = 60
            probe_retries = 6
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("at most 5"), "{error}");
}

#[test]
fn rejects_slo_window_without_target() {
    let error = load(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "api"
            name = "API"
            target = "https://example.com"
            interval_secs = 60
            slo_window_days = 30
        "#,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("requires slo_uptime"), "{error}");
}

/// The shipped example, as `hora check` reads it.
const EXAMPLE: &str = include_str!("../../../../config.example.toml");

/// Uncomment the lines of `text` from the one starting with `from` to the
/// next blank line (`# key = value  # note` becomes `key = value  # note`).
fn uncomment_block(text: &str, from: &str) -> String {
    let start = text
        .find(from)
        .unwrap_or_else(|| panic!("{from:?} not in the example"));
    let end = text[start..]
        .find("\n\n")
        .map_or(text.len(), |at| start + at);
    let block: String = text[start..end]
        .lines()
        .map(|line| line.strip_prefix("# ").unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{}{block}{}", &text[..start], &text[end..])
}

/// Every `[[monitors]]` table of a config, as plain TOML values.
fn raw_monitors(text: &str) -> Vec<toml::Table> {
    let table: toml::Table = toml::from_str(text).expect("example parses as TOML");
    table["monitors"]
        .as_array()
        .expect("monitors array")
        .iter()
        .map(|monitor| monitor.as_table().expect("monitor table").clone())
        .collect()
}

#[test]
fn the_example_config_round_trips_into_typed_monitors() {
    let config = super::parse_with_exec_dir(EXAMPLE, None).expect("the example loads");
    let raw = raw_monitors(EXAMPLE);
    assert_eq!(config.monitors.len(), raw.len());
    for (monitor, table) in config.monitors.iter().zip(&raw) {
        // Kind and target survive as written: the target is the identity
        // peers compare, so it must not be normalized.
        let kind = table
            .get("kind")
            .and_then(toml::Value::as_str)
            .unwrap_or("http");
        assert_eq!(monitor.kind().as_str(), kind, "{}", monitor.id);
        let target = table
            .get("target")
            .and_then(toml::Value::as_str)
            .unwrap_or("");
        assert_eq!(monitor.target(), target, "{}", monitor.id);
        assert_eq!(
            Some(monitor.interval_secs),
            table["interval_secs"]
                .as_integer()
                .and_then(|n| u64::try_from(n).ok())
        );
    }
    let smtp = config.find_monitor("smtp").expect("smtp monitor");
    assert_eq!(
        crate::cert::starttls_of(smtp),
        Some(&Starttls::Smtp { ehlo_name: None })
    );
    assert!(smtp.checks_cert());
    let backup = config.find_monitor("nightly-backup").expect("push monitor");
    assert_eq!(backup.spec, MonitorKind::Push(PushSpec { schedule: None }));
}

#[test]
fn the_example_config_optional_settings_land_in_their_kind() {
    // The commented-out settings of the example, switched on: each lands in
    // its kind's typed settings, parsed.
    let mut text = EXAMPLE.to_owned();
    for from in [
        "# keyword = \"operational\"",
        "# ehlo_name = ",
        "# push_token = ",
        "# [[monitors]]\n# id = \"raid\"",
        "# [[monitors]]\n# id = \"dns-check\"",
    ] {
        text = uncomment_block(&text, from);
    }
    // `${BACKUP_TOKEN}` is unset here: give the token a value of its own.
    let text = text.replace("${BACKUP_TOKEN}", "a-long-enough-backup-token");
    let exec_dir = std::env::temp_dir();
    let config = super::parse_with_exec_dir(&text, Some(exec_dir)).expect("the example loads");
    assert_eq!(config.monitors.len(), raw_monitors(&text).len());

    let api = http_spec(config.find_monitor("api").expect("api"));
    let keyword = api.keyword.as_ref().expect("keyword");
    assert_eq!(
        (keyword.text.as_str(), keyword.invert),
        ("operational", false)
    );
    let json = api.json.as_ref().expect("json assertion");
    assert_eq!(
        (json.query.raw(), json.expected.as_deref()),
        ("$.status", Some("ok"))
    );
    let number = api.number.as_ref().expect("number assertion");
    assert_eq!(
        (number.regex.raw(), number.min, number.max),
        (r#"ships">(\d+)<"#, Some(1), Some(500))
    );
    assert_eq!(api.max_body_kb, Some(256));

    let smtp = config.find_monitor("smtp").expect("smtp");
    assert_eq!(
        crate::cert::starttls_of(smtp),
        Some(&Starttls::Smtp {
            ehlo_name: Some("status.example.com".to_owned())
        })
    );

    let backup = config.find_monitor("nightly-backup").expect("push");
    assert_eq!(
        backup.push_token.as_ref().map(AsRef::as_ref),
        Some("a-long-enough-backup-token")
    );
    let MonitorKind::Push(push) = &backup.spec else {
        panic!("not a push monitor");
    };
    let schedule = push.schedule.as_ref().expect("schedule");
    assert_eq!(
        (schedule.cron.raw(), schedule.grace_secs),
        ("0 3 * * *", 1800)
    );

    let raid = config.find_monitor("raid").expect("exec");
    assert_eq!(
        raid.spec,
        MonitorKind::Exec(ExecSpec {
            program: "check_raid".to_owned(),
            args: vec!["--no-sudo".to_owned()],
        })
    );
    assert_eq!(raid.target(), "");

    let dns = dns_spec(config.find_monitor("dns-check").expect("dns"));
    assert_eq!(dns.record, DnsRecord::A);
    assert_eq!(dns.expected.as_deref(), Some("1.2.3.4"));
    assert_eq!(dns.resolver, Some("8.8.8.8:53".parse().unwrap()));
}

#[test]
fn settings_of_another_kind_that_always_loaded_still_load() {
    // Never refused before the typed monitor, so still accepted - and
    // dropped (with a warning), since this kind has no use for them.
    let config = load(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "db"
            name = "DB"
            kind = "tcp"
            target = "db.example.com:5432"
            interval_secs = 60
            expected_status = 200
            headers = { Accept = "text/html" }
            max_body_kb = 64
        "#,
    )
    .expect("loads as before");
    assert_eq!(
        config.monitors[0].spec,
        MonitorKind::Tcp(TcpSpec {
            target: "db.example.com:5432".to_owned(),
            starttls: None,
            dual_stack: false,
        })
    );
}

#[test]
fn a_monitor_error_reads_as_before() {
    // Validation now runs as the table is read; the message the operator
    // sees is still the monitor's own, without the decoder's decoration.
    let error = load(
        r#"
            [page]
            [server]
            [[monitors]]
            id = "db"
            name = "DB"
            kind = "tcp"
            target = "db.example.com:5432"
            interval_secs = 60
            keyword = "ok"
        "#,
    )
    .unwrap_err()
    .to_string();
    assert_eq!(
        error,
        "monitor db: keyword/json_query/number_regex/proxy require an http monitor"
    );
}

/// `[page] logo` (and `logo_dark`) is read relative to the config file, typed
/// by extension and versioned by its bytes; a missing, oversized or unknown
/// file fails the load, and so does a dark logo without a light one.
#[test]
fn page_logo_is_read_next_to_the_config() {
    let dir = std::env::temp_dir().join(format!("hora-logo-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config_path = dir.join("config.toml");
    let write_config = |logo: &str| {
        std::fs::write(
            &config_path,
            MINIMAL.replace("[page]", &format!("[page]\nlogo = \"{logo}\"")),
        )
        .unwrap();
    };

    std::fs::write(dir.join("mark.svg"), "<svg/>").unwrap();
    write_config("mark.svg");
    let config = super::load_from(&config_path).expect("loads");
    let logo = config.page.logo_image.expect("logo read");
    assert_eq!(logo.content_type, "image/svg+xml");
    assert_eq!(&*logo.bytes, b"<svg/>");
    assert_eq!(logo.version.len(), 12);

    std::fs::write(dir.join("mark.svg"), "<svg></svg>").unwrap();
    let again = super::load_from(&config_path).expect("loads");
    assert_ne!(again.page.logo_image.expect("logo").version, logo.version);

    std::fs::write(dir.join("mark-dark.svg"), "<svg/>").unwrap();
    std::fs::write(
        &config_path,
        MINIMAL.replace(
            "[page]",
            "[page]\nlogo = \"mark.svg\"\nlogo_dark = \"mark-dark.svg\"",
        ),
    )
    .unwrap();
    let both = super::load_from(&config_path).expect("loads");
    assert_eq!(
        &*both.page.logo_dark_image.expect("dark logo").bytes,
        b"<svg/>"
    );

    std::fs::write(
        &config_path,
        MINIMAL.replace("[page]", "[page]\nlogo_dark = \"mark-dark.svg\""),
    )
    .unwrap();
    let err = format!("{:#}", super::load_from(&config_path).unwrap_err());
    assert!(err.contains("logo_dark needs page.logo"), "{err}");

    write_config("missing.png");
    let err = format!("{:#}", super::load_from(&config_path).unwrap_err());
    assert!(err.contains("missing.png"), "{err}");

    std::fs::write(dir.join("mark.gif"), "GIF89a").unwrap();
    write_config("mark.gif");
    let err = format!("{:#}", super::load_from(&config_path).unwrap_err());
    assert!(err.contains(".svg, .png, .webp or .jpg"), "{err}");

    std::fs::write(dir.join("huge.png"), vec![0_u8; 256 * 1024 + 1]).unwrap();
    write_config("huge.png");
    let err = format!("{:#}", super::load_from(&config_path).unwrap_err());
    assert!(err.contains("the limit is"), "{err}");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn pushover_channel_options_are_validated() {
    let channel =
        "\n[[channels]]\nname='pager'\ntype='pushover'\ntoken='test-token'\nuser='test-user'\n";
    assert!(load(&format!("{MINIMAL}{channel}priority=-1\nsound='none'")).is_ok());
    assert!(load(&format!("{MINIMAL}{channel}priority=2")).is_err());
    assert!(
        load(&format!(
            "{MINIMAL}{channel}priority=2\nretry=30\nexpire=600"
        ))
        .is_ok()
    );
}

#[test]
fn jq_is_compiled_at_load_and_options_require_an_expression() {
    assert!(load(&format!("{MINIMAL}\njq='('")).is_err());
    assert!(load(&format!("{MINIMAL}\njq_expected='true'")).is_err());
    assert!(
        load(&format!(
            "{MINIMAL}\njq='.value | tonumber'\njq_operator='>='\njq_expected='2'"
        ))
        .is_ok()
    );
}

#[test]
fn compatibility_probe_fields_are_validated() {
    assert!(
        load(&format!(
            "{MINIMAL}\nexpected_status=200\nexpected_statuses=[201]"
        ))
        .is_err()
    );
    assert!(load(&format!("{MINIMAL}\nexpected_statuses=[600]")).is_err());
    assert!(load(&format!("{MINIMAL}\nmethod='bad method'")).is_err());
    let config = load(&format!(
        "{MINIMAL}\nexpected_statuses=[200,201,302]\nignore_tls=true\nmethod='HEAD'"
    ))
    .unwrap();
    assert!(config.monitors[0].ignore_tls());
    assert_eq!(http_spec(&config.monitors[0]).method, reqwest::Method::HEAD);
    let ping = "[page]\n[server]\n[[monitors]]\nid='ping'\nname='Ping'\nkind='icmp'\ntarget='192.0.2.1'\ninterval_secs=60\nping_count=3\npacket_size=56";
    let config = load(ping).unwrap();
    let MonitorKind::Icmp(spec) = &config.monitors[0].spec else {
        panic!()
    };
    assert_eq!((spec.ping_count, spec.packet_size), (3, 56));
    assert!(load(&ping.replace("ping_count=3", "ping_count=0")).is_err());
}
