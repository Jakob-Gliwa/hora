//! Producer writes: push heartbeats, pushed alerts and the deprecated `?token=`.

use super::*;

#[tokio::test]
async fn push_records_heartbeat_with_token() {
    let res = test_app()
        .await
        .oneshot(push("/api/push/beat?token=s3cret&status=up"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn push_rejects_unknown_status_and_negative_ping() {
    let (app, store) = test_app_with_pool().await;
    for bad in [
        "/api/push/beat?token=s3cret&status=dwon",
        "/api/push/beat?token=s3cret&status=0",
        "/api/push/beat?token=s3cret&ping=-5",
    ] {
        let res = app.clone().oneshot(push(bad)).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
    // Nothing was recorded by the rejected pushes.
    let recorded = hora_core::db::recent_checks(&store, "beat", 10)
        .await
        .unwrap();
    assert!(recorded.is_empty());

    for good in [
        "/api/push/beat?token=s3cret",
        "/api/push/beat?token=s3cret&status=down&ping=0",
        "/api/push/beat?token=s3cret&status=degraded",
    ] {
        let res = app.clone().oneshot(push(good)).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{good}");
    }
}

#[tokio::test]
async fn push_rejects_wrong_token() {
    let res = test_app()
        .await
        .oneshot(push("/api/push/beat?token=wrong"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn push_to_an_unknown_id_answers_like_a_wrong_token() {
    // "web" exists but is an HTTP monitor, not a push target; "nope" does
    // not exist at all. Both read like a wrong token: no id oracle.
    let app = test_app().await;
    for uri in [
        "/api/push/web?token=x",
        "/api/push/nope",
        "/api/push/beat?token=wrong",
    ] {
        let res = app.clone().oneshot(push(uri)).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(body_text(res).await, "invalid push token", "{uri}");
    }
}

#[tokio::test]
async fn push_records_peer_heartbeat() {
    // A watched peer's listen id accepts heartbeats, like a push monitor.
    let res = test_app()
        .await
        .oneshot(push("/api/push/peer-x?token=peertok"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn push_rejects_wrong_peer_token() {
    let res = test_app()
        .await
        .oneshot(push("/api/push/peer-x?token=nope"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

/// A JSON POST to the alert endpoint, with an optional `X-Push-Token` (the
/// item token) and/or `Authorization: Bearer` (the global token).
fn alert(uri: &str, body: &str, push_token: Option<&str>, bearer: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .extension(fake_peer());
    if let Some(token) = push_token {
        builder = builder.header("x-push-token", token);
    }
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder.body(Body::from(body.to_owned())).expect("request")
}

fn deprecated(res: &axum::response::Response) -> bool {
    let deprecation = res.headers().get("deprecation");
    let link = res.headers().get(header::LINK);
    match (deprecation, link) {
        (Some(deprecation), Some(link)) => {
            assert_eq!(deprecation, "true");
            assert_eq!(
                link,
                "<https://uplg.github.io/hora/reference/api/#authentication>; \
                     rel=\"deprecation\""
            );
            true
        }
        (None, None) => false,
        other => panic!("Deprecation and Link go together: {other:?}"),
    }
}

#[tokio::test]
async fn query_tokens_on_writes_are_flagged_deprecated() {
    // Writes authenticated by ?token= still work, flagged as deprecated.
    for (request, expected) in [
        (push("/api/push/beat?token=s3cret"), StatusCode::OK),
        (
            push("/api/event?title=deploy&token=0123456789abcdef"),
            StatusCode::OK,
        ),
        (
            push("/api/silence?monitors=web&duration=10m&token=0123456789abcdef"),
            StatusCode::OK,
        ),
        (
            alert(
                "/api/monitors/web/alert?token=0123456789abcdef",
                r#"{"title":"deploy started"}"#,
                None,
                None,
            ),
            StatusCode::ACCEPTED,
        ),
        // A bad body after a query-token auth is still told to move it.
        (
            push("/api/announce?title=x&severity=panic&token=0123456789abcdef"),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let uri = request.uri().to_string();
        let res = test_app().await.oneshot(request).await.unwrap();
        assert_eq!(res.status(), expected, "{uri}");
        assert!(deprecated(&res), "{uri}");
    }

    // The same writes through headers: no deprecation.
    let mut header_push = push("/api/push/beat");
    header_push
        .headers_mut()
        .insert("x-push-token", HeaderValue::from_static("s3cret"));
    let mut bearer_event = push("/api/event?title=deploy");
    bearer_event.headers_mut().insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer 0123456789abcdef"),
    );
    for request in [
        header_push,
        bearer_event,
        alert(
            "/api/monitors/beat/alert",
            r#"{"title":"t"}"#,
            Some("s3cret"),
            None,
        ),
    ] {
        let uri = request.uri().to_string();
        let res = test_app().await.oneshot(request).await.unwrap();
        assert!(!deprecated(&res), "{uri}");
    }

    // A refused write is not flagged, and read-only views keep ?token=.
    let res = test_app()
        .await
        .oneshot(push("/api/event?title=x&token=wrong"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert!(!deprecated(&res));
    let res = test_app()
        .await
        .oneshot(get("/api/summary?token=0123456789abcdef"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(!deprecated(&res));
}

#[tokio::test]
async fn alert_dispatches_with_item_token_and_records_it() {
    let (app, store) = test_app_with_pool().await;
    let res = app
            .oneshot(alert(
                "/api/monitors/beat/alert",
                r#"{"severity":"error","title":"Patch failed","message":"3/12 ops","tags":{"task_id":"t1"}}"#,
                Some("s3cret"),
                None,
            ))
            .await
            .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);

    let alerts = hora_core::db::recent_pushed_alerts(&store, 10)
        .await
        .unwrap();
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].monitor_id, "beat");
    assert_eq!(alerts[0].severity, "error");
    assert_eq!(alerts[0].title, "Patch failed");
    // The tag is folded into the stored message alongside the producer text.
    assert!(
        alerts[0].message.contains("3/12 ops") && alerts[0].message.contains("task_id=t1"),
        "{}",
        alerts[0].message
    );
}

#[tokio::test]
async fn alert_accepts_global_auth_token_for_a_non_push_monitor() {
    // "web" is an HTTP monitor with no push_token: the global viewer token
    // authorizes, via either the Bearer header or the ?token= query.
    let res = test_app()
        .await
        .oneshot(alert(
            "/api/monitors/web/alert",
            r#"{"title":"deploy started"}"#,
            None,
            Some("0123456789abcdef"),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);

    let res = test_app()
        .await
        .oneshot(alert(
            "/api/monitors/web/alert?token=0123456789abcdef",
            r#"{"title":"deploy started"}"#,
            None,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn alert_rejects_wrong_or_missing_token() {
    for (push_token, bearer) in [(Some("wrong"), None), (None, None), (None, Some("nope"))] {
        let res = test_app()
            .await
            .oneshot(alert(
                "/api/monitors/beat/alert",
                r#"{"title":"x"}"#,
                push_token,
                bearer,
            ))
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::UNAUTHORIZED,
            "{push_token:?} {bearer:?}"
        );
    }
}

#[tokio::test]
async fn alert_does_not_reveal_which_ids_exist() {
    // Without the operator token, an unknown id, a private monitor and a
    // public one all answer the same 401 - no existence oracle.
    for uri in [
        "/api/monitors/nope/alert",
        "/api/monitors/intra/alert",
        "/api/monitors/web/alert",
    ] {
        let res = test_app()
            .await
            .oneshot(alert(uri, r#"{"title":"x"}"#, Some("guess"), None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(
            body_text(res).await,
            "alerting requires the monitor's push_token (X-Push-Token) or server.auth_token",
            "{uri}"
        );
    }
}

#[tokio::test]
async fn alert_unknown_monitor_is_404() {
    let res = test_app()
        .await
        .oneshot(alert(
            "/api/monitors/nope/alert",
            r#"{"title":"x"}"#,
            None,
            Some("0123456789abcdef"),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn alert_rejects_empty_title_and_bad_severity() {
    for body in [r#"{"title":"   "}"#, r#"{"severity":"boom","title":"x"}"#] {
        let res = test_app()
            .await
            .oneshot(alert(
                "/api/monitors/beat/alert",
                body,
                Some("s3cret"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{body}");
    }
}

#[tokio::test]
async fn alert_coalesces_a_repeated_dedup_key() {
    let (app, store) = test_app_with_pool().await;
    let body = r#"{"severity":"error","title":"flap","dedup_key":"ekb:mat"}"#;

    let first = app
        .clone()
        .oneshot(alert(
            "/api/monitors/beat/alert",
            body,
            Some("s3cret"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    assert!(body_text(first).await.contains("dispatched"));

    // Same key inside the window: coalesced, not dispatched or recorded again.
    let second = app
        .clone()
        .oneshot(alert(
            "/api/monitors/beat/alert",
            body,
            Some("s3cret"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::ACCEPTED);
    assert!(body_text(second).await.contains("coalesced"));

    let alerts = hora_core::db::recent_pushed_alerts(&store, 10)
        .await
        .unwrap();
    assert_eq!(alerts.len(), 1, "the repeat must not add a second row");
}

#[tokio::test]
async fn alert_history_respects_public_and_private_visibility() {
    let (app, _pool) = test_app_with_pool().await;
    // A public monitor's alert ("web" has no push_token: global token).
    let res = app
        .clone()
        .oneshot(alert(
            "/api/monitors/web/alert",
            r#"{"severity":"warning","title":"web alert","message":"web detail"}"#,
            None,
            Some("0123456789abcdef"),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    // A private monitor's alert ("intra" is public = false).
    let res = app
        .clone()
        .oneshot(alert(
            "/api/monitors/intra/alert",
            r#"{"severity":"error","title":"intra alert","message":"intra detail"}"#,
            None,
            Some("0123456789abcdef"),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);

    // Anonymous: the public monitor's title shows, its message is collapsed,
    // and the private monitor's alert is hidden outright.
    let anon = body_text(app.clone().oneshot(get("/history")).await.unwrap()).await;
    assert!(anon.contains("web alert"), "{anon}");
    assert!(!anon.contains("web detail"), "{anon}");
    assert!(!anon.contains("intra alert"), "{anon}");

    // Authenticated: full detail, private monitor included.
    let full = body_text(
        app.oneshot(get("/history?token=0123456789abcdef"))
            .await
            .unwrap(),
    )
    .await;
    assert!(
        full.contains("web detail")
            && full.contains("intra alert")
            && full.contains("intra detail"),
        "{full}"
    );
}

#[tokio::test]
async fn kuma_push_get_post_tokens_fractional_ping_and_body() {
    let store = hora_core::db::Store::in_memory().await;
    let config = Arc::new(hora_core::config::parse("[page]\n[server]\n[[monitors]]\nid='job'\nname='Job'\nkind='push'\ninterval_secs=60\nkuma_push=true\npush_token='synthetic-credential'").unwrap());
    let client = hora_core::http::client(None).unwrap();
    let notifier = hora_core::notifications::shared(&config, &client);
    let (_tx, rx) = watch::channel(config);
    let app = router(AppState::new(
        store.clone(),
        rx,
        Arc::new(AtomicU64::new(0)),
        notifier,
    ));
    for method in ["GET", "POST"] {
        let req = axum::http::Request::builder()
            .method(method)
            .uri("/api/push/synthetic-credential?status=down&msg=failed&ping=12.5")
            .extension(fake_peer())
            .body(Body::from("producer body is optional"))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        let bytes = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["ok"],
            true
        );
    }
    let checks = hora_core::db::recent_checks(&store, "job", 10)
        .await
        .unwrap();
    assert!(!checks.is_empty());
    assert_eq!(checks[0].status, hora_core::status::CheckStatus::Down);
    assert_eq!(checks[0].latency_ms, Some(12));
    for token in ["wrong", "job"] {
        let response = app
            .clone()
            .oneshot(get(&format!("/api/push/{token}")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    assert_eq!(
        super::super::logged_path("/api/push/secret"),
        "/api/push/[redacted]"
    );
}

#[tokio::test]
async fn the_config_monitor_cannot_be_pushed_from_outside() {
    // Loaded leniently, as the daemon does, so the built-in monitor exists.
    let config = Arc::new(hora_core::config::parse_lenient("[page]\n[server]\n").unwrap());
    assert!(
        config
            .find_monitor(hora_core::config::CONFIG_MONITOR_ID)
            .is_some()
    );
    let store = hora_core::db::Store::in_memory().await;
    let client = hora_core::http::client(None).unwrap();
    let notifier = hora_core::notifications::shared(&config, &client);
    let (_tx, rx) = watch::channel(config);
    let app = router(AppState::new(
        store.clone(),
        rx,
        Arc::new(AtomicU64::new(fresh_tick())),
        notifier,
    ));
    for method_uri in ["/api/push/hora-config?status=up", "/api/push/hora-config"] {
        let res = app.clone().oneshot(push(method_uri)).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{method_uri}");
    }
    let recorded = hora_core::db::recent_checks(&store, "hora-config", 10)
        .await
        .unwrap();
    assert!(recorded.is_empty());
}
