use anyhow::{bail, Context, Result};
use askama::Template;
use axum::body::{to_bytes, Body};
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::response::sse::Event;
use axum::response::{IntoResponse, Response, Sse};
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
use crate::db::{App, Db, DeleteVersion, Revision};
use crate::logs::LogStore;
use crate::manifest::Manifest;
use crate::runtime::Runtime;
use crate::secrets::Secrets;
use http_body_util::BodyExt;

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
        .route(
            "/api/apps/{name}",
            get(api_app_detail).delete(api_delete_app),
        )
        .route("/api/apps/{name}/deploy", post(api_deploy))
        .route("/api/apps/{name}/rollback", post(api_rollback))
        .route("/api/apps/{name}/versions", get(api_versions))
        .route(
            "/api/apps/{name}/versions/{version}/activate",
            post(api_activate_version),
        )
        .route(
            "/api/apps/{name}/versions/{version}",
            axum::routing::delete(api_delete_version),
        )
        .route("/api/apps/{name}/deactivate", post(api_deactivate))
        .route("/api/apps/{name}/secrets", get(api_list_secrets))
        .route(
            "/api/apps/{name}/secrets/{key}",
            put(api_set_secret).delete(api_delete_secret),
        )
        .route("/api/tokens", post(api_create_token))
        .route("/api/secrets/global", get(api_list_global_secrets))
        .route("/auth/device", get(auth_device))
        .route(
            "/api/secrets/global/{key}",
            put(api_set_global_secret).delete(api_delete_global_secret),
        )
        .route("/api/apps/{name}/logs", get(api_logs))
        .route("/api/apps/{name}/logs/stream", get(api_logs_stream))
        .route("/auth/login", get(auth_login))
        .route("/auth/callback", get(auth_callback))
        .route("/auth/logout", get(auth_logout))
        .fallback(dispatch)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::local_auth::gate,
        ))
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
        _ => match s.auth.verify_api_token(headers) {
            Some(u) => Ok(u),
            None => Err((StatusCode::UNAUTHORIZED, "unauthorized").into_response()),
        },
    }
}

fn require_browser_user(
    s: &AppState,
    headers: &HeaderMap,
    _host: &str,
    return_to: &str,
) -> Result<User, Response> {
    if s.auth.dev_bypass(headers) {
        return Ok(User::dev());
    }
    if let Some(u) = s.auth.user_from_headers(headers) {
        return Ok(u);
    }
    if s.auth.local.is_some() {
        let target = format!(
            "/auth/login?return_to={}",
            crate::auth::urlencoding(return_to)
        );
        return Err(axum::response::Redirect::to(&target).into_response());
    }
    if s.auth.oidc_enabled() {
        return Err(login_redirect(&s.auth, auth_host_of(headers), return_to));
    }
    Err((StatusCode::UNAUTHORIZED, "authentication required").into_response())
}

fn err400(e: anyhow::Error) -> Response {
    (StatusCode::BAD_REQUEST, format!("{e:#}")).into_response()
}

fn err500(e: anyhow::Error) -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response()
}

// ---------- API ----------

async fn api_list_apps(State(s): State<S>, headers: HeaderMap) -> Result<Response, Response> {
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
    // Delete only directories belonging to the removed DB snapshot. A concurrent
    // deployment may already have published a new, uniquely named directory.
    for dir in
        s.db.delete_app(&name)
            .map_err(|e| (StatusCode::CONFLICT, format!("{e:#}")).into_response())?
    {
        cleanup_version(&s, &dir);
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Cookie-authenticated browser mutations must be non-simple requests. With
/// no CORS permission, another origin (including a sibling app) cannot send this
/// header. Bearer clients do not rely on ambient browser credentials.
async fn require_version_mutation(s: &AppState, headers: &HeaderMap) -> Result<User, Response> {
    let user = require_api_user(s, headers).await?;
    if !s.auth.dev_bypass(headers) && s.auth.user_from_headers(headers).is_some() {
        if headers.get("x-pushit-csrf").and_then(|h| h.to_str().ok()) != Some("1") {
            return Err((
                StatusCode::FORBIDDEN,
                "browser version mutations require X-PushIt-CSRF: 1",
            )
                .into_response());
        }
        if let Some(origin) = headers.get(header::ORIGIN) {
            let same_origin = origin
                .to_str()
                .ok()
                .and_then(|v| v.parse::<http::Uri>().ok())
                .is_some_and(|origin| {
                    matches!(origin.scheme_str(), Some("http" | "https"))
                        && origin.authority().is_some_and(|authority| {
                            authority
                                .as_str()
                                .eq_ignore_ascii_case(auth_host_of(headers))
                        })
                });
            if !same_origin {
                return Err((
                    StatusCode::FORBIDDEN,
                    "browser version mutation origin must match Host",
                )
                    .into_response());
            }
        }
    }
    Ok(user)
}

fn cleanup_version(s: &AppState, dir: &str) {
    s.runtime.evict(std::path::Path::new(dir));
    if let Err(error) = std::fs::remove_dir_all(dir) {
        if error.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(%error, %dir, "version removed from database but artifact cleanup failed");
        }
    }
}

async fn api_versions(
    State(s): State<S>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Response, Response> {
    require_api_user(&s, &headers).await?;
    if s.db.get_app(&name).map_err(err500)?.is_none() {
        return Err((StatusCode::NOT_FOUND, "no such app").into_response());
    }
    let versions: Vec<_> = s
        .db
        .revisions(&name)
        .map_err(err500)?
        .into_iter()
        .map(|r| serde_json::json!({"id": r.id, "created_at": r.created_at, "active": r.active}))
        .collect();
    Ok(axum::Json(versions).into_response())
}

async fn api_deactivate(
    State(s): State<S>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Response, Response> {
    require_version_mutation(&s, &headers).await?;
    if !s.db.deactivate(&name).map_err(err500)? {
        return Err((StatusCode::NOT_FOUND, "no such app").into_response());
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn api_delete_version(
    State(s): State<S>,
    headers: HeaderMap,
    Path((name, version)): Path<(String, String)>,
) -> Result<Response, Response> {
    require_version_mutation(&s, &headers).await?;
    // DB snapshot + per-artifact busy-check are serialized with reader pins.
    match s.db.delete_version(&name, &version).map_err(err500)? {
        DeleteVersion::Missing => Err((StatusCode::NOT_FOUND, "no such version").into_response()),
        DeleteVersion::Busy => Err((
            StatusCode::CONFLICT,
            "version has in-flight requests; retry deletion after they finish",
        )
            .into_response()),
        DeleteVersion::Active => Err((
            StatusCode::CONFLICT,
            "cannot delete the active version; deactivate it or activate another version first",
        )
            .into_response()),
        DeleteVersion::Deleted(dir) => {
            cleanup_version(&s, &dir);
            Ok(StatusCode::NO_CONTENT.into_response())
        }
    }
}

async fn api_activate_version(
    State(s): State<S>,
    headers: HeaderMap,
    Path((name, version)): Path<(String, String)>,
) -> Result<Response, Response> {
    require_version_mutation(&s, &headers).await?;
    activate_version(&s, &name, &version)?;
    Ok(axum::Json(serde_json::json!({"app": name, "version": version})).into_response())
}

fn activate_version(s: &AppState, name: &str, version: &str) -> Result<(), Response> {
    let (snapshot, _artifacts) =
        s.db.pin_revision(name, version)
            .map_err(err500)?
            .ok_or_else(|| (StatusCode::NOT_FOUND, "no such version").into_response())?;
    let manifest = snapshot.manifest.as_deref().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "legacy version has no manifest; redeploy the desired version before activation",
        )
            .into_response()
    })?;
    let manifest = Manifest::parse(manifest).map_err(err400)?;
    validate_manifest_secrets(s, name, &manifest).map_err(err400)?;
    if !std::path::Path::new(&snapshot.artifact_dir)
        .join("app.wasm")
        .is_file()
    {
        return Err((StatusCode::BAD_REQUEST, "version artifact is missing").into_response());
    }
    s.runtime
        .load(std::path::Path::new(&snapshot.artifact_dir))
        .map_err(err400)?;
    s.db.activate(name, version).map_err(err400)?;
    Ok(())
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
    validate_manifest_secrets(s, name, &manifest)?;

    let rev = new_revision_id();
    let dir = s.cfg.apps_dir().join(name).join(&rev);
    std::fs::create_dir_all(s.cfg.apps_dir().join(name))?;
    // Never reuse an artifact directory: even a revision-id collision must fail.
    std::fs::create_dir(&dir)?;
    unpack_archive(archive, &dir).inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&dir);
    })?;
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
    s.db.deploy_revision(name, &rev, &dir.to_string_lossy(), &domain, manifest_yaml)
        .inspect_err(|_| {
            let _ = std::fs::remove_dir_all(&dir);
        })?;

    if let Err(error) = s
        .logs
        .append(name, format!("deployed revision {rev}\n").as_bytes())
    {
        tracing::warn!(%error, "revision committed but deployment log could not be written");
    }
    Ok(serde_json::json!({
        "app": name, "revision": rev, "domain": domain,
    }))
}

fn validate_manifest_secrets(s: &AppState, name: &str, manifest: &Manifest) -> Result<()> {
    // secrets validation: all declared names must exist
    let existing = s.db.secret_names(name)?;
    let missing: Vec<&String> = manifest
        .secrets
        .iter()
        .filter(|k| !existing.contains(k))
        .collect();
    if !missing.is_empty() {
        bail!(
            "missing secrets: {}",
            missing
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // sql bindings: sources must exist now, not just at request time
    for (conn, b) in &manifest.sql_bindings {
        if manifest.capabilities.sql.iter().all(|g| g != conn) {
            continue; // binding for a non-granted name is harmless dead config
        }
        let (src, key) = b
            .split_once(':')
            .filter(|(s, k)| {
                matches!(*s, "secret" | "platform") && !k.is_empty()
            })
            .with_context(|| format!("sql binding '{conn}': '{b}' must be secret:<app-secret-key> or platform:<global-secret-name>"))?;
        let ok = match src {
            "secret" => s.db.get_secret_enc(name, key)?.is_some(),
            _ => s.db.get_global_secret(key)?.is_some(),
        };
        if !ok {
            bail!(
                "sql binding '{conn}' → {src} secret '{key}' not set ({})",
                if src == "secret" {
                    format!("pushit secret set {name} {key}=<dsn>")
                } else {
                    format!("pushit secret global set {key}=<dsn>")
                }
            );
        }
    }

    Ok(())
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
        // Revision contents must not point outside their immutable directory.
        if p.is_absolute()
            || p.components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            || !(e.header().entry_type().is_file() || e.header().entry_type().is_dir())
        {
            bail!("archive contains an unsafe path or non-regular entry");
        }
        if !e.unpack_in(dest)? {
            bail!("archive entry escapes revision directory");
        }
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

// platform-wide (global) secrets

/// Mint a long-lived CLI token for the authenticated user.
async fn api_create_token(State(s): State<S>, headers: HeaderMap) -> Result<Response, Response> {
    let user = require_api_user(&s, &headers).await?;
    Ok(axum::Json(serde_json::json!({
        "token": s.auth.sign_api_token(&user),
        "user": user.email,
    }))
    .into_response())
}

/// Browser-side step of `pushit login`: the browser carries the session
/// cookie, the controller bounces the minted long-lived token to the CLI's
/// localhost listener. Unauthenticated browsers are redirected through
/// Keycloak first (return_to keeps the query string).
async fn auth_device(
    State(s): State<S>,
    headers: HeaderMap,
    Query(q): Query<DeviceQuery>,
) -> Response {
    let host = auth_host_of(&headers);
    let user =
        match require_browser_user(&s, &headers, host, &format!("/auth/device?port={}", q.port)) {
            Ok(u) => u,
            Err(r) => return r,
        };
    let token = s.auth.sign_api_token(&user);
    let url = format!(
        "http://127.0.0.1:{}/?token={token}&user={}",
        q.port,
        crate::auth::urlencoding(&user.email)
    );
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, url)
        .body(Body::empty())
        .unwrap()
}

#[derive(Deserialize)]
struct DeviceQuery {
    port: u16,
}

// rollback

async fn api_list_global_secrets(
    State(s): State<S>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    require_api_user(&s, &headers).await?;
    let keys = s.db.global_secret_names().map_err(err500)?;
    Ok(axum::Json(serde_json::json!({ "keys": keys })).into_response())
}

async fn api_set_global_secret(
    State(s): State<S>,
    headers: HeaderMap,
    Path(key): Path<String>,
    body: Bytes,
) -> Result<Response, Response> {
    require_api_user(&s, &headers).await?;
    let enc = s.secrets.encrypt(&body).map_err(err500)?;
    s.db.set_global_secret(&key, &enc).map_err(err500)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn api_delete_global_secret(
    State(s): State<S>,
    headers: HeaderMap,
    Path(key): Path<String>,
) -> Result<Response, Response> {
    require_api_user(&s, &headers).await?;
    s.db.delete_global_secret(&key).map_err(err500)?;
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
        None => {
            s.db.previous_revision(&name)
                .map_err(err500)?
                .ok_or_else(|| (StatusCode::BAD_REQUEST, "no previous revision").into_response())?
        }
    };
    activate_version(&s, &name, &rev)?;
    if let Err(error) = s
        .logs
        .append(&name, format!("rolled back to {rev}\n").as_bytes())
    {
        tracing::warn!(%error, "rollback committed but deployment log could not be written");
    }
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

async fn auth_login(State(s): State<S>, headers: HeaderMap, Query(q): Query<LoginQ>) -> Response {
    let host = auth_host_of(&headers);
    if s.auth.dev_bypass(&headers) {
        let cookie = s.auth.cookie_header(&User::dev());
        let dest = q.return_to.unwrap_or_else(|| "/".into());
        return Response::builder()
            .status(StatusCode::FOUND)
            .header(header::SET_COOKIE, cookie)
            .header(header::LOCATION, dest)
            .body(Body::empty())
            .unwrap();
    }
    if !s.auth.oidc_enabled() {
        return (StatusCode::UNAUTHORIZED, "authentication required").into_response();
    }
    login_redirect(&s.auth, host, &q.return_to.unwrap_or_else(|| "/".into()))
}

fn login_redirect(auth: &Auth, host: &str, return_to: &str) -> Response {
    match auth.auth_url(host, return_to) {
        Ok((url, cookie)) => Response::builder()
            .status(StatusCode::FOUND)
            .header(header::LOCATION, url)
            .header(header::SET_COOKIE, cookie)
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
    let host = auth_host_of(&headers);
    match (q.code, q.state) {
        (Some(code), Some(st)) => match s.auth.exchange_code(host, &code, &st, &headers).await {
            Ok((user, return_to)) => Response::builder()
                .status(StatusCode::FOUND)
                .header(header::SET_COOKIE, s.auth.cookie_header(&user))
                .header(header::SET_COOKIE, s.auth.clear_login_cookie())
                .header(header::LOCATION, return_to)
                .body(Body::empty())
                .unwrap(),
            Err(_) => (StatusCode::BAD_REQUEST, "invalid login callback").into_response(),
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

fn auth_host_of(headers: &HeaderMap) -> &str {
    headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

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
                        port_suffix: local_port_suffix(s),
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
        port_suffix: local_port_suffix(s),
        revs: &revs,
        active: revs.iter().any(|r| r.active),
        logs,
        secrets,
    }
    .render()
    .map(|h| axum::response::Html(h).into_response())
    .unwrap_or_else(|e| err500(e.into()))
}

async fn app_dispatch(s: &AppState, req: Request<Body>, host: &str, _prefix: &str) -> Response {
    let Some((app, rev, artifacts)) = s.db.pin_active_by_domain(host).ok().flatten() else {
        return (StatusCode::NOT_FOUND, "no app for this host").into_response();
    };
    let uri = req.uri().clone();
    let user = match require_browser_user(s, req.headers(), host, &uri.to_string()) {
        Ok(u) => u,
        Err(r) => return r,
    };
    if uri.path().starts_with("/_fn/") || uri.path() == "/_fn" {
        let artifacts = Arc::new(artifacts);
        let response = call_fn(s, &app, &rev, user, req, artifacts.clone()).await;
        // Keep the pin through streaming response consumption, not merely until
        // the handler returns its headers. Preserve frames (including trailers).
        return pin_response(response, artifacts);
    }
    serve_static(&rev, &uri)
}

fn pin_response(
    response: Response,
    artifacts: Arc<tokio::sync::OwnedRwLockReadGuard<()>>,
) -> Response {
    let (parts, body) = response.into_parts();
    let pinned = body.map_frame(move |frame| {
        let _ = &artifacts;
        frame
    });
    Response::from_parts(parts, Body::new(pinned))
}

fn serve_static(rev: &Revision, uri: &http::Uri) -> Response {
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

async fn call_fn(
    s: &AppState,
    app: &App,
    rev: &Revision,
    user: User,
    req: Request<Body>,
    artifact_pin: Arc<tokio::sync::OwnedRwLockReadGuard<()>>,
) -> Response {
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
        if s.auth.local.is_some() {
            if k == header::AUTHORIZATION || k.as_str().starts_with("x-user-") {
                continue;
            }
            if k == header::COOKIE {
                if let Ok(value) = v.to_str() {
                    let retained = value
                        .split(';')
                        .filter(|part| {
                            !part
                                .trim()
                                .split('=')
                                .next()
                                .unwrap_or("")
                                .trim_start_matches("__Host-")
                                .starts_with("pushit_")
                        })
                        .collect::<Vec<_>>()
                        .join(";");
                    if !retained.is_empty() {
                        hreq = hreq.header(k, retained);
                    }
                }
                continue;
            }
        }
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
    let (sql_allowed, sql_dsns) = {
        let bindings = &manifest.sql_bindings;
        match crate::sql_host::resolve_grants(&manifest.capabilities.sql, &app.name, |name| {
            let b = bindings.get(name)?;
            let (src, key) = b.split_once(':')?;
            let enc = match src {
                "secret" => s.db.get_secret_enc(&app.name, key),
                "platform" => s.db.get_global_secret(key),
                _ => return None,
            };
            enc.ok()
                .flatten()
                .and_then(|enc| s.secrets.decrypt(&enc).ok())
                .map(|v| String::from_utf8_lossy(&v).into_owned())
        }) {
            Ok(r) => r,
            Err(e) => return err500(e),
        }
    };
    let blob_stores = match crate::blob_host::load_blobstores(&s.cfg.data_dir) {
        Ok(m) => m,
        Err(e) => return err500(e),
    };
    let caps = crate::runtime::HostCaps {
        artifact_pin: Some(artifact_pin),
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
        .call(
            std::path::Path::new(&rev.artifact_dir),
            &app.name,
            &s.cfg.data_dir,
            env,
            caps,
            hreq,
        )
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

fn local_port_suffix(s: &AppState) -> String {
    if s.cfg.owner_auth() {
        s.cfg.auth_port_suffix()
    } else {
        String::new()
    }
}

// ---------- hub templates ----------

#[derive(Template)]
#[template(path = "hub_index.html")]
struct HubIndex<'a> {
    port_suffix: String,
    user: &'a str,
    apps: Vec<crate::db::AppRow>,
}

#[derive(Template)]
#[template(path = "hub_app.html")]
struct HubApp<'a> {
    port_suffix: String,
    app: &'a App,
    revs: &'a [crate::db::Revision],
    active: bool,
    logs: Vec<String>,
    secrets: Vec<String>,
}

#[cfg(test)]
mod lifetime_tests {
    use super::*;

    #[test]
    fn response_body_retains_pin_independently_of_guest_task_until_dropped() {
        let db = Db::open(std::path::Path::new(":memory:")).unwrap();
        db.deploy_revision("a", "r1", "/a/r1", "a.localhost", "name: a")
            .unwrap();
        let (_, _, pin) = db.pin_active_by_domain("a.localhost").unwrap().unwrap();
        let body = Body::from_stream(futures::stream::pending::<Result<Bytes, Infallible>>());
        let response = pin_response(Response::new(body), Arc::new(pin));
        db.deploy_revision("a", "r2", "/a/r2", "a.localhost", "name: a")
            .unwrap();
        // No guest exists in this test: only the streaming response owns a pin.
        assert_eq!(db.delete_version("a", "r1").unwrap(), DeleteVersion::Busy);
        drop(response);
        assert_eq!(
            db.delete_version("a", "r1").unwrap(),
            DeleteVersion::Deleted("/a/r1".into())
        );
    }
}
