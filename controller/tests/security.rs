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
use std::{sync::Arc, time::Duration};
use tower::ServiceExt;

fn state(dev: bool) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        data_dir: dir.path().to_owned(),
        base_domain: "localhost".into(),
        bind: "127.0.0.1:0".into(),
        dev,
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
