//! Route-level regression tests deliberately run without PUSHIT_DEV or an OIDC provider.
//! The old client header and no-OIDC browser fallback must never authenticate production.
use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use pushit_controller::{
    auth::{Auth, User},
    config::Config,
    db::Db,
    logs::LogStore,
    runtime::Runtime,
    secrets::Secrets,
    server::{router, AppState},
};
use std::{convert::Infallible, sync::Arc, time::Duration};
use tower::ServiceExt;

fn state(dev: bool) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        data_dir: dir.path().to_owned(),
        base_domain: "localhost".into(),
        bind: "127.0.0.1:0".into(),
        dev,
        auth_mode: pushit_controller::config::AuthMode::Oidc,
        oidc_issuer: None,
        oidc_client_id: None,
        oidc_client_secret: None,
        session_key: b"test-only-session-key-at-least-32-bytes".to_vec(),
        handler_timeout_secs: 30,
    };
    let state = Arc::new(AppState {
        db: Db::open(&cfg.db_path()).unwrap(),
        secrets: Secrets::load(&cfg.age_key_path()).unwrap(),
        runtime: Runtime::new(Duration::from_secs(30)).unwrap(),
        logs: Arc::new(LogStore::new(cfg.logs_dir()).unwrap()),
        auth: Auth::new(cfg.clone()),
        cfg,
    });
    (dir, state)
}

#[tokio::test]
async fn production_rejects_forged_dev_header_on_api_and_browser_routes() {
    let (_dir, state) = state(false);
    let app = router(state.clone());
    for (method, path) in [
        ("GET", "/api/apps"),
        ("DELETE", "/api/apps/victim"),
        ("POST", "/api/apps/victim/deploy"),
        ("POST", "/api/apps/victim/rollback"),
        ("GET", "/api/apps/victim/versions"),
        ("POST", "/api/apps/victim/deactivate"),
        ("POST", "/api/apps/victim/versions/r1/activate"),
        ("DELETE", "/api/apps/victim/versions/r1"),
        ("PUT", "/api/apps/victim/secrets/KEY"),
        ("DELETE", "/api/apps/victim/secrets/KEY"),
        ("PUT", "/api/secrets/global/KEY"),
        ("DELETE", "/api/secrets/global/KEY"),
        ("POST", "/api/tokens"),
        ("GET", "/auth/login"),
        ("GET", "/auth/device?port=12345"),
        ("GET", "/"),
    ] {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "hub.localhost")
            .header("x-pushit-dev", "1")
            .header(header::CONTENT_TYPE, "multipart/form-data; boundary=test")
            .body(Body::from("--test--\r\n"))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {path}"
        );
        assert!(!response.headers().contains_key(header::SET_COOKIE));
    }
    assert!(state.db.secret_names("victim").unwrap().is_empty());
    assert!(state.db.global_secret_names().unwrap().is_empty());
    assert!(state.db.list_apps().unwrap().is_empty());
}

#[tokio::test]
async fn production_accepts_valid_session_and_api_token() {
    let (_dir, state) = state(false);
    let app = router(state.clone());
    let user = User {
        id: "user-1".into(),
        name: "User".into(),
        email: "user@example.test".into(),
    };
    let bearer = format!("Bearer {}", state.auth.sign_api_token(&user));
    let cookie = format!("pushit_session={}", state.auth.sign_session(&user));
    for (name, value) in [
        (header::AUTHORIZATION, bearer),
        (header::COOKIE, cookie.clone()),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/tokens")
                    .header(name, value)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 8192).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let minted = state
            .auth
            .verify_session(body["token"].as_str().unwrap())
            .unwrap();
        assert_eq!(minted.id, user.id);
    }
    let response = app
        .oneshot(
            Request::builder()
                .uri("/")
                .header(header::HOST, "hub.localhost")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn explicitly_enabled_dev_mode_still_works_without_client_header() {
    let (_dir, state) = state(true);
    let app = router(state.clone());
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/apps/example/secrets/KEY")
                .body(Body::from("value"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let encrypted = state.db.get_secret_enc("example", "KEY").unwrap().unwrap();
    assert_eq!(state.secrets.decrypt(&encrypted).unwrap(), b"value");
    for path in ["/auth/login", "/auth/device?port=12345"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header(header::HOST, "localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FOUND, "{path}");
    }
    let response = app
        .oneshot(
            Request::builder()
                .uri("/")
                .header(header::HOST, "hub.localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn oidc_callback_rejects_missing_or_unknown_state_without_a_session() {
    let (_dir, state) = state(false);
    let app = router(state);
    for path in [
        "/auth/callback",
        "/auth/callback?code=fake",
        "/auth/callback?code=fake&state=unknown",
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header(header::HOST, "localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
        // A failed callback must not mint a session, even if it clears the login cookie.
        for cookie in response.headers().get_all(header::SET_COOKIE) {
            assert!(!cookie.to_str().unwrap().starts_with("pushit_session="));
        }
    }
}

#[tokio::test]
async fn rollback_restores_manifest_routes_and_static_config_but_not_secret_values() {
    use pushit_controller::manifest::Manifest;
    let (_dir, state) = state(true);
    let app = router(state.clone());
    let manifest_a = "name: example\ndomain: old\nenv: {RELEASE: A}\nsecrets: [OLD_KEY]\ncapabilities:\n  http_out: []\n  sql: [db]\n  blobstore: [files]\nsql_bindings: {db: 'secret:OLD_KEY'}\n";
    let manifest_b = "name: example\ndomain: new\nenv: {RELEASE: B}\nsecrets: [NEW_KEY]\ncapabilities:\n  sql: [analytics]\nsql_bindings: {analytics: 'platform:NEW_DB'}\n";
    for (id, domain, manifest) in [
        ("a", "old.localhost", manifest_a),
        ("b", "new.localhost", manifest_b),
    ] {
        let parsed = Manifest::parse(manifest).unwrap();
        let artifact = state.cfg.apps_dir().join("example").join(id);
        std::fs::create_dir_all(artifact.join("dist")).unwrap();
        std::fs::write(artifact.join("dist/index.html"), format!("revision-{id}")).unwrap();
        std::fs::write(
            artifact.join("dist/config.js"),
            format!(
                "window.__ENV__ = {};\n",
                serde_json::to_string(&parsed.env).unwrap()
            ),
        )
        .unwrap();
        // Synthetic static artifacts isolate snapshot/routing behavior from WASM compilation.
        state
            .db
            .deploy_revision("example", id, artifact.to_str().unwrap(), domain, manifest)
            .unwrap();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/config.js")
                    .header(header::HOST, domain)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), 8192).await.unwrap(),
            format!(
                "window.__ENV__ = {};\n",
                serde_json::to_string(&parsed.env).unwrap()
            )
        );
    }
    let old_value = state.secrets.encrypt(b"old-password").unwrap();
    state
        .db
        .set_secret("example", "OLD_KEY", &old_value)
        .unwrap();
    let rotated = state.secrets.encrypt(b"rotated-password").unwrap();
    state.db.set_secret("example", "OLD_KEY", &rotated).unwrap();
    state.db.set_global_secret("NEW_DB", &rotated).unwrap();

    state.db.activate("example", "a").unwrap();
    let (snapshot, revision) = state
        .db
        .active_app_by_domain("old.localhost")
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.manifest, manifest_a);
    assert_eq!(revision.manifest.as_deref(), Some(manifest_a));
    assert_eq!(revision.id, "a");
    assert_eq!(revision.domain.as_deref(), Some("old.localhost"));
    let restored = Manifest::parse(&snapshot.manifest).unwrap();
    assert_eq!(restored.env["RELEASE"], "A");
    assert_eq!(restored.secrets, ["OLD_KEY"]);
    assert!(restored.http_out());
    assert_eq!(restored.capabilities.sql, ["db"]);
    assert_eq!(restored.capabilities.blobstore, ["files"]);
    assert_eq!(restored.sql_bindings["db"], "secret:OLD_KEY");
    assert_eq!(
        state
            .db
            .revisions("example")
            .unwrap()
            .iter()
            .find(|r| r.id == "b")
            .unwrap()
            .manifest
            .as_deref(),
        Some(manifest_b)
    );

    for (path, expected) in [
        ("/", "revision-a"),
        ("/config.js", "window.__ENV__ = {\"RELEASE\":\"A\"};\n"),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header(header::HOST, "old.localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), 8192).await.unwrap(),
            expected
        );
    }
    let response = app
        .oneshot(
            Request::builder()
                .uri("/config.js")
                .header(header::HOST, "new.localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let secret = state
        .db
        .get_secret_enc("example", "OLD_KEY")
        .unwrap()
        .unwrap();
    assert_eq!(state.secrets.decrypt(&secret).unwrap(), b"rotated-password");
    assert_eq!(
        state.db.get_global_secret("NEW_DB").unwrap().unwrap(),
        rotated
    );
}

#[tokio::test]
async fn version_mutations_require_browser_csrf_header_and_same_origin() {
    let (_dir, state) = state(false);
    state
        .db
        .deploy_revision(
            "example",
            "r1",
            "/unused",
            "example.localhost",
            "name: example",
        )
        .unwrap();
    let user = User::dev();
    let cookie = format!("pushit_session={}", state.auth.sign_session(&user));
    let app = router(state.clone());
    for path in [
        "/api/apps/example/deactivate",
        "/api/apps/example/versions/r1/activate",
        "/api/apps/example/versions/r1",
    ] {
        let method = if path.ends_with("/r1") {
            "DELETE"
        } else {
            "POST"
        };
        for (csrf, origin) in [
            (false, "http://hub.localhost"),
            (true, "http://evil.localhost"),
            (true, "null"),
        ] {
            let mut request = Request::builder()
                .method(method)
                .uri(path)
                .header(header::HOST, "hub.localhost")
                .header(header::COOKIE, &cookie)
                .header(header::ORIGIN, origin);
            if csrf {
                request = request.header("x-pushit-csrf", "1");
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{method} {path}");
        }
    }
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/apps/example/deactivate")
                .header(header::HOST, "hub.localhost")
                .header(header::COOKIE, &cookie)
                .header(header::ORIGIN, "http://hub.localhost")
                .header("x-pushit-csrf", "1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/apps/example/deactivate")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", state.auth.sign_api_token(&user)),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn inactive_versions_are_not_routed_and_errors_are_useful() {
    let (_dir, state) = state(true);
    let artifact = state.cfg.apps_dir().join("example/r1");
    std::fs::create_dir_all(artifact.join("dist")).unwrap();
    std::fs::write(artifact.join("dist/index.html"), "example").unwrap();
    state
        .db
        .deploy_revision(
            "example",
            "r1",
            artifact.to_str().unwrap(),
            "example.localhost",
            "name: example",
        )
        .unwrap();
    let app = router(state.clone());
    let request = |method: &str, path: &str| {
        Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "example.localhost")
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        app.clone()
            .oneshot(request("GET", "/"))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let response = app
        .clone()
        .oneshot(request("DELETE", "/api/apps/example/versions/r1"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = to_bytes(response.into_body(), 8192).await.unwrap();
    assert!(String::from_utf8_lossy(&body).contains("active version"));
    assert_eq!(
        app.clone()
            .oneshot(request("POST", "/api/apps/example/deactivate"))
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    for path in ["/", "/config.js", "/_fn/hello"] {
        assert_eq!(
            app.clone()
                .oneshot(request("GET", path))
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        app.clone()
            .oneshot(request(
                "POST",
                "/api/apps/example/versions/missing/activate"
            ))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.clone()
            .oneshot(request("POST", "/api/apps/example/versions/r1/activate"))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert!(state.db.active_revision("example").unwrap().is_none());
    assert_eq!(
        app.clone()
            .oneshot(request("DELETE", "/api/apps/example/versions/r1"))
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    assert!(!artifact.exists());
    assert!(state.db.get_app("example").unwrap().is_some());
    assert_eq!(
        app.oneshot(request("POST", "/api/apps/missing/deactivate"))
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn in_flight_http_body_pins_only_its_version_and_cancel_releases_it() {
    let (_dir, state) = state(true);
    let old = state.cfg.apps_dir().join("example/r1");
    let current = state.cfg.apps_dir().join("example/r2");
    let other = state.cfg.apps_dir().join("other/r1");
    for (dir, text) in [(&old, "old"), (&current, "current"), (&other, "other")] {
        std::fs::create_dir_all(dir.join("dist")).unwrap();
        std::fs::write(dir.join("dist/index.html"), text).unwrap();
    }
    state
        .db
        .deploy_revision(
            "example",
            "r1",
            old.to_str().unwrap(),
            "example.localhost",
            "name: example",
        )
        .unwrap();
    state
        .db
        .deploy_revision(
            "other",
            "r1",
            other.to_str().unwrap(),
            "other.localhost",
            "name: other",
        )
        .unwrap();
    let app = router(state.clone());
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (_finish_tx, finish_rx) = tokio::sync::oneshot::channel::<()>();
    let body = Body::from_stream(futures::stream::once(async move {
        // Body polling happens only after routing has atomically pinned r1.
        started_tx.send(()).unwrap();
        let _ = finish_rx.await;
        Ok::<_, Infallible>(axum::body::Bytes::new())
    }));
    let handler = tokio::spawn(
        app.clone().oneshot(
            Request::builder()
                .method("POST")
                .uri("/_fn/hello")
                .header(header::HOST, "example.localhost")
                .body(body)
                .unwrap(),
        ),
    );
    tokio::time::timeout(Duration::from_secs(5), started_rx)
        .await
        .unwrap()
        .unwrap();
    state
        .db
        .deploy_revision(
            "example",
            "r2",
            current.to_str().unwrap(),
            "example.localhost",
            "name: example",
        )
        .unwrap();
    let request = |method: &str, path: &str| {
        Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "example.localhost")
            .body(Body::empty())
            .unwrap()
    };
    let response = app
        .clone()
        .oneshot(request("DELETE", "/api/apps/example/versions/r1"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(
        String::from_utf8_lossy(&to_bytes(response.into_body(), 8192).await.unwrap())
            .contains("in-flight")
    );
    assert!(old.exists());
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        app.clone().oneshot(request("GET", "/")),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(response.into_body(), 8192).await.unwrap(),
        "current"
    );
    assert_eq!(
        app.clone()
            .oneshot(request("DELETE", "/api/apps/example"))
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        app.clone().oneshot(
            Request::builder()
                .uri("/")
                .header(header::HOST, "other.localhost")
                .body(Body::empty())
                .unwrap(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(to_bytes(response.into_body(), 8192).await.unwrap(), "other");
    handler.abort();
    assert!(handler.await.unwrap_err().is_cancelled());
    assert_eq!(
        app.oneshot(request("DELETE", "/api/apps/example/versions/r1"))
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    assert!(!old.exists());
    assert!(current.exists());
    assert_eq!(
        state.db.active_revision("example").unwrap().unwrap().id,
        "r2"
    );
}
