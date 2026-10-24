//! Real HTTP coverage: no OIDC provider, proxy or development bypass.
use pushit_controller::{
    auth::Auth,
    config::{AuthMode, Config},
    db::Db,
    local_auth::LocalAuth,
    logs::LogStore,
    runtime::Runtime,
    secrets::Secrets,
    server::{router, AppState},
};
use reqwest::{header, Client, Method, Response};
use std::{sync::Arc, time::Duration};
const PASSWORD: &str = "synthetic-http-test-password";
struct Server {
    _dir: tempfile::TempDir,
    state: Arc<AppState>,
    client: Client,
    port: u16,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        LocalAuth::setup(dir.path(), PASSWORD).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let cfg = Config {
            data_dir: dir.path().into(),
            base_domain: "localhost".into(),
            bind: format!("127.0.0.1:{port}"),
            dev: false,
            auth_mode: AuthMode::Local,
            oidc_issuer: None,
            oidc_client_id: None,
            oidc_client_secret: None,
            session_key: vec![],
            handler_timeout_secs: 30,
        };
        let state = Arc::new(AppState {
            db: Db::open(&cfg.db_path()).unwrap(),
            secrets: Secrets::load(&cfg.age_key_path()).unwrap(),
            runtime: Runtime::new(Duration::from_secs(30)).unwrap(),
            logs: Arc::new(LogStore::new(cfg.logs_dir()).unwrap()),
            auth: Auth::try_new(cfg.clone()).unwrap(),
            cfg,
        });
        for app in ["first", "second"] {
            let artifact = dir.path().join(app);
            std::fs::create_dir_all(artifact.join("dist")).unwrap();
            std::fs::write(artifact.join("dist/index.html"), app).unwrap();
            state
                .db
                .deploy_revision(
                    app,
                    "r1",
                    artifact.to_str().unwrap(),
                    &format!("{app}.localhost"),
                    &format!("name: {app}"),
                )
                .unwrap();
        }
        let app = router(state.clone());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            _dir: dir,
            state,
            client: Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            port,
            task,
        }
    }
    fn request(&self, method: Method, host: &str, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("http://127.0.0.1:{}{path}", self.port))
            .header(header::HOST, format!("{host}.localhost:{}", self.port))
    }
    fn origin(&self, host: &str) -> String {
        format!("http://{host}.localhost:{}", self.port)
    }
    async fn login(&self) -> String {
        let page = self
            .request(Method::GET, "hub", "/auth/login")
            .send()
            .await
            .unwrap();
        assert_eq!(page.status(), 200);
        let challenge = cookie(&page, "pushit_local_login");
        assert!(!page.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Domain="));
        let csrf = challenge.split_once('=').unwrap().1;
        let result = self
            .request(Method::POST, "hub", "/auth/login")
            .header(header::ORIGIN, self.origin("hub"))
            .header(header::COOKIE, &challenge)
            .form(&[("password", PASSWORD), ("csrf", csrf), ("return_to", "/")])
            .send()
            .await
            .unwrap();
        assert_eq!(result.status(), 303);
        cookie(&result, "pushit_local_owner")
    }
    async fn token(&self) -> serde_json::Value {
        let r = self
            .request(Method::POST, "hub", "/auth/local/token")
            .header("x-pushit-cli", "1")
            .json(&serde_json::json!({"password":PASSWORD}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        r.json().await.unwrap()
    }
}
fn cookie(response: &Response, name: &str) -> String {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap())
        .find(|v| v.starts_with(&format!("{name}=")))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .into()
}
fn location(response: &Response) -> String {
    response.headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .into()
}
fn path(url: &str) -> String {
    let u = reqwest::Url::parse(url).unwrap();
    format!(
        "{}{}",
        u.path(),
        u.query().map(|q| format!("?{q}")).unwrap_or_default()
    )
}

#[tokio::test]
async fn local_hub_login_handoff_isolation_csrf_logout_over_http() {
    let s = Server::start().await;
    assert_eq!(
        s.request(Method::GET, "hub", "/api/apps")
            .header("x-pushit-dev", "1")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let owner = s.login().await;
    let hub = s
        .request(Method::GET, "hub", "/")
        .header(header::COOKIE, &owner)
        .send()
        .await
        .unwrap();
    assert_eq!(hub.status(), 200);
    assert!(hub
        .text()
        .await
        .unwrap()
        .contains(&format!("//first.localhost:{}", s.port)));
    for origin in [None, Some(s.origin("first")), Some("null".into())] {
        let mut r = s
            .request(Method::PUT, "hub", "/api/secrets/global/TEST")
            .header(header::COOKIE, &owner)
            .header("x-pushit-csrf", "1")
            .body("synthetic");
        if let Some(o) = origin {
            r = r.header(header::ORIGIN, o)
        }
        assert_eq!(r.send().await.unwrap().status(), 403);
    }
    assert!(s.state.db.global_secret_names().unwrap().is_empty());
    assert_eq!(
        s.request(Method::PUT, "hub", "/api/secrets/global/TEST")
            .header(header::COOKIE, &owner)
            .header(header::ORIGIN, s.origin("hub"))
            .header("x-pushit-csrf", "1")
            .body("synthetic")
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    // No password form (or owner cookie) ever visits the application origin.
    let begin = s
        .request(Method::GET, "first", "/auth/login?return_to=%2F")
        .send()
        .await
        .unwrap();
    assert_eq!(begin.status(), 303);
    let challenge = cookie(&begin, "pushit_local_challenge");
    let authorize = location(&begin);
    assert!(authorize.starts_with(&s.origin("hub")));
    assert!(!authorize.contains("token="));
    let authorized = s
        .request(Method::GET, "hub", &path(&authorize))
        .header(header::COOKIE, &owner)
        .send()
        .await
        .unwrap();
    assert_eq!(authorized.status(), 303);
    let complete = path(&location(&authorized));
    assert_eq!(authorized.headers()["referrer-policy"], "no-referrer");
    for (host, c) in [("second", Some(&challenge)), ("first", None)] {
        let mut r = s.request(Method::GET, host, &complete);
        if let Some(c) = c {
            r = r.header(header::COOKIE, c)
        }
        assert_eq!(r.send().await.unwrap().status(), 403);
    }
    let completed = s
        .request(Method::GET, "first", &complete)
        .header(header::COOKIE, &challenge)
        .send()
        .await
        .unwrap();
    assert_eq!(completed.status(), 303);
    assert_eq!(location(&completed), "/");
    let app = cookie(&completed, "pushit_local_app");
    assert!(!completed.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .contains("Domain="));
    assert_eq!(
        s.request(Method::GET, "first", &complete)
            .header(header::COOKIE, &challenge)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        s.request(Method::GET, "first", "/")
            .header(header::COOKIE, &app)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "first"
    );
    assert_eq!(
        s.request(Method::GET, "second", "/")
            .header(header::COOKIE, &app)
            .send()
            .await
            .unwrap()
            .status(),
        303
    );
    assert_eq!(
        s.request(Method::GET, "hub", "/api/apps")
            .header(
                header::COOKIE,
                app.replace("pushit_local_app", "pushit_local_owner")
            )
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        s.request(Method::GET, "hub", "/api/apps")
            .header(header::COOKIE, format!("{owner}; {owner}"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    for origin in [None, Some(s.origin("second")), Some("null".into())] {
        let mut r = s
            .request(Method::POST, "first", "/")
            .header(header::COOKIE, &app);
        if let Some(o) = origin {
            r = r.header(header::ORIGIN, o)
        }
        assert_eq!(r.send().await.unwrap().status(), 403);
    }
    assert_eq!(
        s.request(Method::POST, "first", "/")
            .header(header::COOKIE, &app)
            .header(header::ORIGIN, s.origin("first"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    // A logout GET is only confirmation; cross-origin POST cannot log out the owner.
    let page = s
        .request(Method::GET, "hub", "/auth/logout")
        .header(header::COOKIE, &owner)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    let body = page.text().await.unwrap();
    let csrf = body
        .split("value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    assert_eq!(
        s.request(Method::POST, "hub", "/auth/logout")
            .header(header::COOKIE, &owner)
            .header(header::ORIGIN, s.origin("second"))
            .form(&[("csrf", csrf)])
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        s.request(Method::POST, "hub", "/auth/logout")
            .header(header::COOKIE, &owner)
            .header(header::ORIGIN, s.origin("hub"))
            .form(&[("csrf", csrf)])
            .send()
            .await
            .unwrap()
            .status(),
        303
    );
    assert_eq!(
        s.request(Method::GET, "hub", "/api/apps")
            .header(header::COOKIE, &owner)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        s.request(Method::GET, "first", "/")
            .header(header::COOKIE, &app)
            .send()
            .await
            .unwrap()
            .status(),
        303
    );
}

#[tokio::test]
async fn local_cli_token_login_management_revoke_and_bad_hosts_over_http() {
    let s = Server::start().await;
    for origin in [Some(s.origin("second")), Some("null".into())] {
        let mut r = s
            .request(Method::POST, "hub", "/auth/local/token")
            .header("x-pushit-cli", "1")
            .json(&serde_json::json!({"password":PASSWORD}));
        if let Some(origin) = origin {
            r = r.header(header::ORIGIN, origin)
        }
        assert_eq!(r.send().await.unwrap().status(), 403);
    }
    assert_eq!(
        s.request(Method::POST, "hub", "/auth/local/token")
            .form(&[("password", PASSWORD)])
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        s.request(Method::POST, "hub", "/auth/local/token")
            .header("x-pushit-cli", "1")
            .json(&serde_json::json!({"password":"wrong"}))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let token = s.token().await;
    let value = token["token"].as_str().unwrap();
    let id = token["id"].as_str().unwrap();
    assert_eq!(
        s.request(Method::GET, "hub", "/api/apps")
            .bearer_auth(value)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        s.request(Method::POST, "hub", "/api/tokens")
            .bearer_auth(value)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        s.request(Method::GET, "hub", "/auth/device?port=12345")
            .bearer_auth(value)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    for host in ["first", "second", "unknown"] {
        assert_eq!(
            s.request(Method::GET, host, "/api/apps")
                .bearer_auth(value)
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
    }
    for host in [
        format!("hub.localhost:{}", s.port + 1),
        "evil.example".into(),
        "hub.localhost.evil".into(),
        format!("user@hub.localhost:{}", s.port),
    ] {
        assert_eq!(
            s.client
                .get(format!("http://127.0.0.1:{}/api/apps", s.port))
                .header(header::HOST, host)
                .bearer_auth(value)
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
    }
    assert_eq!(
        s.request(Method::GET, "hub", "/api/apps")
            .header(header::COOKIE, format!("pushit_local_owner={value}"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let list = s
        .request(Method::GET, "hub", "/api/tokens")
        .bearer_auth(value)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(list.contains(id));
    assert!(!list.contains(value));
    assert_eq!(
        s.request(Method::DELETE, "hub", &format!("/api/tokens/{id}"))
            .bearer_auth(value)
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    assert_eq!(
        s.request(Method::GET, "hub", "/api/apps")
            .bearer_auth(value)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
}
