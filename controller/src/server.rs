use anyhow::{bail, Context, Result};
use askama::Template;
use axum::body::{to_bytes, Body};
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response, Sse};
use axum::response::sse::Event;
use axum::routing::{get, post, put};
use axum::Router;
use http_body_util::Full;
use hyper::body::Bytes;
use serde::Deserialize;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;

use crate::auth::{Auth, User};
use crate::config::Config;
use crate::db::{App, Db};
use crate::logs::LogStore;
use crate::manifest::Manifest;
use crate::runtime::Runtime;
use crate::secrets::Secrets;

pub struct AppState {
    pub cfg: Config,
    pub db: Db,
    pub secrets: Secrets,
    pub runtime: Runtime,
    pub logs: Arc<LogStore>,
    pub auth: Auth,
}

type S = Arc<AppState>;

pub fn router(state: S) -> Router {
    Router::new()
        .route("/api/apps", get(api_list_apps))
        .route("/api/apps/{name}", get(api_app_detail).delete(api_delete_app))
        .route("/api/apps/{name}/deploy", post(api_deploy))
        .route("/api/apps/{name}/rollback", post(api_rollback))
        .route(
            "/api/apps/{name}/secrets",
            get(api_list_secrets),
        )
        .route(
            "/api/apps/{name}/secrets/{key}",
            put(api_set_secret).delete(api_delete_secret),
        )
        .route("/api/apps/{name}/logs", get(api_logs))
        .route("/api/apps/{name}/logs/stream", get(api_logs_stream))
        .route("/auth/login", get(auth_login))
        .route("/auth/callback", get(auth_callback))
        .route("/auth/logout", get(auth_logout))
        .fallback(dispatch)
        .with_state(state)
}

// ---------- auth helpers ----------

async fn require_api_user(s: &AppState, headers: &HeaderMap) -> Result<User, Response> {
    if s.auth.dev_bypass(headers) {
        return Ok(User::dev());
    }
    if let Some(u) = s.auth.user_from_headers(headers) {
        return Ok(u);
    }
    match s.auth.verify_bearer(headers).await {
        Ok(Some(u)) => Ok(u),
        _ => Err((StatusCode::UNAUTHORIZED, "unauthorized").into_response()),
    }
}

fn require_browser_user(
    s: &AppState,
    headers: &HeaderMap,
    host: &str,
    return_to: &str,
) -> Result<User, Response> {
    if s.auth.dev_bypass(headers) {
        return Ok(User::dev());
    }
    if let Some(u) = s.auth.user_from_headers(headers) {
        return Ok(u);
    }
    if s.auth.oidc_enabled() {
        let url = s
            .auth
            .auth_url(host, return_to)
            .unwrap_or_else(|_| "/".into());
        return Err(Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, url)
            .body(Body::empty())
            .unwrap());
    }
    // no OIDC configured and not dev: treat as dev user (local install)
    Ok(User::dev())
}

fn err400(e: anyhow::Error) -> Response {
    (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response()
}

fn err500(e: anyhow::Error) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response()
}

// ---------- API ----------

async fn api_list_apps(
    State(s): State<S>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    require_api_user(&s, &headers).await?;
    match s.db.list_apps() {
        Ok(apps) => Ok(axum::Json(apps).into_response()),
        Err(e) => Err(err500(e)),
    }
}

async fn api_app_detail(
    State(s): State<S>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Response, Response> {
    require_api_user(&s, &headers).await?;
    let Some(app) = s.db.get_app(&name).map_err(err500)? else {
        return Err((StatusCode::NOT_FOUND, "no such app").into_response());
    };
    let revs = s.db.revisions(&name).map_err(err500)?;
    Ok(axum::Json(serde_json::json!({
        "name": app.name, "domain": app.domain,
        "manifest": app.manifest, "created_at": app.created_at,
        "revisions": revs,
    }))
    .into_response())
}

async fn api_delete_app(
    State(s): State<S>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Response, Response> {
    require_api_user(&s, &headers).await?;
    s.db.delete_app(&name).map_err(err500)?;
    let dir = s.cfg.apps_dir().join(&name);
    let _ = std::fs::remove_dir_all(dir);
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn api_deploy(
    State(s): State<S>,
    headers: HeaderMap,
    Path(name): Path<String>,
    mut mp: Multipart,
) -> Result<Response, Response> {
    require_api_user(&s, &headers).await?;
    let (manifest_yaml, archive) = read_deploy_fields(&mut mp).await.map_err(err400)?;
    deploy_inner(&s, &name, &manifest_yaml, &archive)
        .map(|j| axum::Json(j).into_response())
        .map_err(err400)
}

async fn read_deploy_fields(mp: &mut Multipart) -> Result<(String, Vec<u8>)> {
    let mut manifest = None;
    let mut archive = None;
    while let Some(f) = mp.next_field().await? {
        match f.name() {
            Some("manifest") => manifest = Some(f.text().await?),
            Some("archive") => archive = Some(f.bytes().await?.to_vec()),
            _ => {}
        }
    }
    Ok((
        manifest.context("missing 'manifest' field")?,
        archive.context("missing 'archive' field")?,
    ))
}

pub fn deploy_inner(
    s: &AppState,
    name: &str,
    manifest_yaml: &str,
    archive: &[u8],
) -> Result<serde_json::Value> {
    let manifest = Manifest::parse(manifest_yaml)?;
    if manifest.name != name {
        bail!("manifest name '{}' != path name '{name}'", manifest.name);
    }
    // secrets validation: all declared names must exist
    let existing = s.db.secret_names(name)?;
    let missing: Vec<&String> = manifest
        .secrets
        .iter()
        .filter(|k| !existing.contains(k))
        .collect();
    if !missing.is_empty() {
        bail!("missing secrets: {}", missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "));
    }

    let rev = new_revision_id();
    let dir = s.cfg.apps_dir().join(name).join(&rev);
    std::fs::create_dir_all(&dir)?;
    unpack_archive(archive, &dir)?;
    if !dir.join("app.wasm").exists() {
        let _ = std::fs::remove_dir_all(&dir);
        bail!("archive lacks app.wasm");
    }

    // validate component links + exports incoming-handler before recording
    s.runtime.load(&dir).inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&dir);
    })?;

    let dist = dir.join("dist");
    if dist.is_dir() {
        let cfg_js = format!(
            "window.__ENV__ = {};\n",
            serde_json::to_string(&manifest.env)?
        );
        std::fs::write(dist.join("config.js"), cfg_js)?;
    }

    let domain = format!("{}.{}", manifest.route_name(), s.cfg.base_domain);
    s.db.upsert_app(name, &domain, manifest_yaml)?;
    s.db.add_revision(name, &rev, &dir.to_string_lossy(), true)?;
    s.db.activate(name, &rev)?;

    s.logs.append(name, format!("deployed revision {rev}\n").as_bytes())?;
    Ok(serde_json::json!({
        "app": name, "revision": rev, "domain": domain,
    }))
}

fn new_revision_id() -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("r{n:x}")
}

fn unpack_archive(bytes: &[u8], dest: &std::path::Path) -> Result<()> {
    let tar = flate2::read::GzDecoder::new(bytes);
    let mut ar = tar::Archive::new(tar);
    for e in ar.entries()? {
        let mut e = e?;
        let p = e.path()?.to_path_buf();
        // reject path escapes
        if p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
            continue;
        }
        e.unpack_in(dest)?;
    }
    Ok(())
}

// secrets

async fn api_list_secrets(
    State(s): State<S>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Response, Response> {
    require_api_user(&s, &headers).await?;
    let keys = s.db.secret_names(&name).map_err(err500)?;
    Ok(axum::Json(serde_json::json!({ "keys": keys })).into_response())
}

async fn api_set_secret(
    State(s): State<S>,
    headers: HeaderMap,
    Path((name, key)): Path<(String, String)>,
    body: Bytes,
) -> Result<Response, Response> {
    require_api_user(&s, &headers).await?;
    let enc = s.secrets.encrypt(&body).map_err(err500)?;
    s.db.set_secret(&name, &key, &enc).map_err(err500)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn api_delete_secret(
    State(s): State<S>,
    headers: HeaderMap,
    Path((name, key)): Path<(String, String)>,
) -> Result<Response, Response> {
    require_api_user(&s, &headers).await?;
    s.db.delete_secret(&name, &key).map_err(err500)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// rollback

#[derive(Deserialize)]
struct RollbackReq {
    revision: Option<String>,
}

async fn api_rollback(
    State(s): State<S>,
    headers: HeaderMap,
    Path(name): Path<String>,
    body: Bytes,
) -> Result<Response, Response> {
    require_api_user(&s, &headers).await?;
    let target = if body.is_empty() {
        None
    } else {
        serde_json::from_slice::<RollbackReq>(&body)
            .map_err(|_| (StatusCode::BAD_REQUEST, "bad body").into_response())?
            .revision
    };
    let rev = match target {
        Some(r) => r,
        None => s
            .db
            .previous_revision(&name)
            .map_err(err500)?
            .ok_or_else(|| (StatusCode::BAD_REQUEST, "no previous revision").into_response())?,
    };
    s.db.activate(&name, &rev).map_err(err400)?;
    s.logs
        .append(&name, format!("rolled back to {rev}\n").as_bytes())
        .map_err(err500)?;
    Ok(axum::Json(serde_json::json!({"app": name, "revision": rev})).into_response())
}

// logs

#[derive(Deserialize)]
struct LogsQ {
    tail: Option<usize>,
}

async fn api_logs(
    State(s): State<S>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Query(q): Query<LogsQ>,
) -> Result<Response, Response> {
    require_api_user(&s, &headers).await?;
    let lines = s.logs.tail(&name, q.tail.unwrap_or(200)).map_err(err500)?;
    Ok(axum::Json(serde_json::json!({"lines": lines})).into_response())
}

async fn api_logs_stream(
    State(s): State<S>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Response, Response> {
    require_api_user(&s, &headers).await?;
    let rx = s.logs.subscribe(&name);
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Ok(line) => Some((Ok::<_, Infallible>(Event::default().data(line)), rx)),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                Some((Ok(Event::default().comment("lagged")), rx))
            }
            Err(_) => None,
        }
    });
    Ok(Sse::new(stream).into_response())
}

// ---------- auth routes ----------

#[derive(Deserialize)]
struct LoginQ {
    return_to: Option<String>,
}

async fn auth_login(
    State(s): State<S>,
    headers: HeaderMap,
    Query(q): Query<LoginQ>,
) -> Response {
    let host = host_of(&headers);
    if s.auth.dev_bypass(&headers) || !s.auth.oidc_enabled() {
        let cookie = s.auth.cookie_header(&User::dev());
        let dest = q.return_to.unwrap_or_else(|| "/".into());
        return Response::builder()
            .status(StatusCode::FOUND)
            .header(header::SET_COOKIE, cookie)
            .header(header::LOCATION, dest)
            .body(Body::empty())
            .unwrap();
    }
    match s.auth.auth_url(&host, &q.return_to.unwrap_or_else(|| "/".into())) {
        Ok(url) => Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, url)
            .body(Body::empty())
            .unwrap(),
        Err(e) => err500(e),
    }
}

#[derive(Deserialize)]
struct CbQ {
    code: Option<String>,
    state: Option<String>,
}

async fn auth_callback(State(s): State<S>, headers: HeaderMap, Query(q): Query<CbQ>) -> Response {
    let host = host_of(&headers);
    match (q.code, q.state) {
        (Some(code), Some(st)) => match s.auth.exchange_code(&host, &code, &st).await {
            Ok((user, return_to)) => Response::builder()
                .status(StatusCode::FOUND)
                .header(header::SET_COOKIE, s.auth.cookie_header(&user))
                .header(header::LOCATION, return_to)
                .body(Body::empty())
                .unwrap(),
            Err(e) => err500(e),
        },
        _ => (StatusCode::BAD_REQUEST, "missing code/state").into_response(),
    }
}

async fn auth_logout(State(s): State<S>) -> Response {
    let cookie = format!(
        "pushit_session=; Domain=.{domain}; Path=/; Max-Age=0",
        domain = s.cfg.base_domain
    );
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::SET_COOKIE, cookie)
        .header(header::LOCATION, "/")
        .body(Body::empty())
        .unwrap()
}

// ---------- host dispatch ----------

fn host_of(headers: &HeaderMap) -> String {
    headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default()
        .to_string()
}

fn route_prefix<'a>(host: &'a str, base: &str) -> Option<&'a str> {
    host.strip_suffix(base)?
        .strip_suffix('.')
        .or(if host == base { Some("") } else { None })
}

async fn dispatch(State(s): State<S>, req: Request<Body>) -> Response {
    let host = host_of(req.headers());
    let prefix = route_prefix(&host, &s.cfg.base_domain).unwrap_or(&host);
    if prefix == "hub" {
        return hub_dispatch(&s, req, &host).await;
    }
    // API on bare domain already handled by router; anything else → app
    app_dispatch(&s, req, &host, prefix).await
}

async fn hub_dispatch(s: &AppState, req: Request<Body>, host: &str) -> Response {
    let uri = req.uri().clone();
    match require_browser_user(s, req.headers(), host, &uri.to_string()) {
        Ok(user) => match (req.method().clone(), uri.path().to_string()) {
            (m, p) if m == http::Method::GET && (p == "/" || p.is_empty()) => {
                match s.db.list_apps() {
                    Ok(apps) => HubIndex {
                        user: &user.name,
                        apps,
                    }
                    .render()
                    .map(|h| axum::response::Html(h).into_response())
                    .unwrap_or_else(|e| err500(e.into())),
                    Err(e) => err500(e),
                }
            }
            (m, p) if m == http::Method::GET && p.starts_with("/apps/") => {
                let name = p.trim_start_matches("/apps/").to_string();
                hub_app_page(s, &name)
            }
            _ => (StatusCode::NOT_FOUND, "not found").into_response(),
        },
        Err(r) => r,
    }
}

fn hub_app_page(s: &AppState, name: &str) -> Response {
    let Some(app) = s.db.get_app(name).ok().flatten() else {
        return (StatusCode::NOT_FOUND, "no such app").into_response();
    };
    let revs = s.db.revisions(name).unwrap_or_default();
    let logs = s.logs.tail(name, 100).unwrap_or_default();
    let secrets = s.db.secret_names(name).unwrap_or_default();
    HubApp {
        app: &app,
        revs: &revs,
        logs,
        secrets,
    }
    .render()
    .map(|h| axum::response::Html(h).into_response())
    .unwrap_or_else(|e| err500(e.into()))
}

async fn app_dispatch(s: &AppState, req: Request<Body>, host: &str, prefix: &str) -> Response {
    let Some(app) = s.db.app_by_domain(prefix).ok().flatten() else {
        return (StatusCode::NOT_FOUND, "no app for this host").into_response();
    };
    let uri = req.uri().clone();
    let user = match require_browser_user(s, req.headers(), host, &uri.to_string()) {
        Ok(u) => u,
        Err(r) => return r,
    };
    if uri.path().starts_with("/_fn/") || uri.path() == "/_fn" {
        return call_fn(&s, &app, user, req).await;
    }
    serve_static(&s, &app, &uri)
}

fn serve_static(s: &AppState, app: &App, uri: &http::Uri) -> Response {
    let Some(rev) = s.db.active_revision(&app.name).ok().flatten() else {
        return (StatusCode::NOT_FOUND, "no active revision").into_response();
    };
    let dist = PathBuf::from(&rev.artifact_dir).join("dist");
    let mut rel = uri.path().trim_start_matches('/').to_string();
    if rel.is_empty() {
        rel = "index.html".into();
    }
    // path traversal guard
    if rel.split('/').any(|seg| seg == "..") {
        return (StatusCode::BAD_REQUEST, "bad path").into_response();
    }
    let mut path = dist.join(&rel);
    if path.is_dir() {
        path = path.join("index.html");
    }
    if !path.is_file() {
        // SPA fallback
        path = dist.join("index.html");
        if !path.is_file() {
            return (StatusCode::NOT_FOUND, "not found").into_response();
        }
    }
    let body = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => return err500(e.into()),
    };
    let mime = mime_guess::from_path(&path)
        .first_or_octet_stream()
        .to_string();
    let cache = if rel.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, cache)
        .body(Body::from(body))
        .unwrap()
}

async fn call_fn(s: &AppState, app: &App, user: User, req: Request<Body>) -> Response {
    let Some(rev) = s.db.active_revision(&app.name).ok().flatten() else {
        return (StatusCode::NOT_FOUND, "no active revision").into_response();
    };
    let Ok(manifest) = Manifest::parse(&app.manifest) else {
        return err500(anyhow::anyhow!("stored manifest fails to parse"));
    };

    // resolve env: manifest env + decrypted secrets
    let mut env: Vec<(String, String)> = manifest
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for k in &manifest.secrets {
        if let Ok(Some(enc)) = s.db.get_secret_enc(&app.name, k) {
            if let Ok(v) = s.secrets.decrypt(&enc) {
                env.push((k.clone(), String::from_utf8_lossy(&v).into_owned()));
            }
        }
    }

    let (parts, body) = req.into_parts();
    let bytes = match to_bytes(body, 64 << 20).await {
        Ok(b) => b,
        Err(e) => return err500(e.into()),
    };
    let mut hreq = hyper::Request::builder()
        .method(parts.method)
        .uri(parts.uri);
    for (k, v) in &parts.headers {
        hreq = hreq.header(k, v);
    }
    hreq = hreq
        .header("x-user-id", &user.id)
        .header("x-user-name", &user.name)
        .header("x-user-email", &user.email);
    let hreq = match hreq.body(Full::new(bytes)) {
        Ok(r) => r,
        Err(e) => return err500(e.into()),
    };
    let (sql_allowed, sql_dsns) =
        match crate::sql_host::resolve_grants(&manifest.capabilities.sql, &app.name, |key| {
            s.db
                .get_secret_enc(&app.name, key)
                .ok()
                .flatten()
                .and_then(|enc| s.secrets.decrypt(&enc).ok())
                .map(|v| String::from_utf8_lossy(&v).into_owned())
        }) {
            Ok(r) => r,
            Err(e) => return err500(e),
        };
    let blob_stores = match crate::blob_host::load_blobstores(&s.cfg.data_dir) {
        Ok(m) => m,
        Err(e) => return err500(e),
    };
    let caps = crate::runtime::HostCaps {
        allow_http_out: manifest.http_out(),
        data_dir: s.cfg.data_dir.clone(),
        sql_allowed,
        sql_dsns,
        blob_allowed: manifest.capabilities.blobstore.clone(),
        blob_stores,
        log: crate::log_host::LogCtx {
            app: app.name.clone(),
            logs: s.logs.clone(),
        },
    };
    match s
        .runtime
        .call(std::path::Path::new(&rev.artifact_dir), &app.name, &s.cfg.data_dir, env, caps, hreq)
        .await
    {
        Ok(res) => {
            let _ = s.logs.append(&app.name, &res.stdout);
            let _ = s.logs.append(&app.name, &res.stderr);
            match res.response {
                Some(resp) => {
                    let (parts, body) = resp.into_parts();
                    let mut b = Response::builder().status(parts.status);
                    for (k, v) in &parts.headers {
                        b = b.header(k, v);
                    }
                    b.body(Body::new(body))
                        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
                }
                None => StatusCode::NO_CONTENT.into_response(),
            }
        }
        Err(e) => {
            let _ = s
                .logs
                .append(&app.name, format!("handler error: {e:#}\n").as_bytes());
            (StatusCode::BAD_GATEWAY, format!("{e:#}")).into_response()
        }
    }
}

// ---------- hub templates ----------

#[derive(Template)]
#[template(path = "hub_index.html")]
struct HubIndex<'a> {
    user: &'a str,
    apps: Vec<crate::db::AppRow>,
}

#[derive(Template)]
#[template(path = "hub_app.html")]
struct HubApp<'a> {
    app: &'a App,
    revs: &'a [crate::db::Revision],
    logs: Vec<String>,
    secrets: Vec<String>,
}
