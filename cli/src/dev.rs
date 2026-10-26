//! `pushit dev` — standalone local runtime.
//!
//! Reuses the controller's wasmtime host (`pushit_controller::runtime`) so
//! local behavior matches production: same wasi:sql/keyvalue/blobstore/logging/http
//! host functions, same timeout machinery. No OIDC, no revisions, no
//! controller process — env comes from the manifest + `--env`, SQL DSNs
//! for bound connections from `--dsn`.

use crate::{find_wasm, manifest_path, run_build_steps};
use anyhow::{bail, Context, Result};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use pushit_controller::log_host::LogCtx;
use pushit_controller::logs::LogStore;
use pushit_controller::manifest::{HostFilter, HttpOut};
use pushit_controller::runtime::{HostCaps, Runtime};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

type FullBody = Full<Bytes>;

#[derive(clap::Args)]
pub struct DevArgs {
    /// Path to pushit.yaml.
    #[arg(long, default_value = "pushit.yaml")]
    pub manifest: PathBuf,
    /// Port for the local server.
    #[arg(long, default_value_t = 9101)]
    pub port: u16,
    /// Env var for the component: --env KEY=value (repeatable).
    #[arg(long = "env")]
    pub env: Vec<String>,
    /// DSN for a sql connection: --dsn name='postgres://…' (repeatable).
    #[arg(long = "dsn")]
    pub dsn: Vec<String>,
    /// Allow all outgoing HTTP, overriding the manifest allowlist.
    #[arg(long)]
    pub http_out: bool,
}

pub fn run(args: &DevArgs) -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting tokio runtime")?
        .block_on(run_async(args))
}

async fn run_async(args: &DevArgs) -> Result<()> {
    let manifest_abs = args
        .manifest
        .canonicalize()
        .with_context(|| format!("read {}", args.manifest.display()))?;
    let manifest_text = std::fs::read_to_string(&manifest_abs)?;
    let yaml: serde_yaml::Value =
        serde_yaml::from_str(&manifest_text).context("parse pushit.yaml")?;
    let manifest = pushit_controller::manifest::Manifest::parse(&manifest_text)?;
    let base = manifest_abs.parent().unwrap().to_path_buf();

    println!("==> build");
    run_build_steps(&yaml, &base)?;

    let wasm = match manifest_path(&yaml, "entry", &base).filter(|p| p.is_file()) {
        Some(p) => p,
        None => find_wasm(&base)?,
    };
    if !wasm.is_file() {
        bail!("no component wasm found — check `entry` in pushit.yaml");
    }
    let dist = manifest_path(&yaml, "ui_dist", &base).filter(|p| p.is_dir());

    // runtime dir: fresh per launch so the wasmtime compile cache never
    // serves a stale component after a rebuild
    let data_dir = base.join(".pushit-dev");
    std::fs::create_dir_all(&data_dir)?;
    let dir = data_dir.join(format!(
        "run-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis()
    ));
    std::fs::create_dir_all(&dir)?;
    std::fs::copy(&wasm, dir.join("app.wasm"))
        .with_context(|| format!("copy {}", wasm.display()))?;

    let logs = Arc::new(LogStore::new(data_dir.join("logs"))?);

    let mut env: Vec<(String, String)> = manifest
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    for e in &args.env {
        let (k, v) = e
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("--env expects KEY=value, got '{e}'"))?;
        env.retain(|(k2, _)| k2 != k);
        env.push((k.to_string(), v.to_string()));
    }
    let declared: Vec<_> = manifest.secrets.iter().collect();
    if !declared.is_empty() {
        let provided: Vec<_> = args
            .env
            .iter()
            .filter_map(|e| e.split('=').next())
            .collect();
        let missing: Vec<_> = declared
            .iter()
            .filter(|s| !provided.contains(&&s[..]))
            .collect();
        if !missing.is_empty() {
            println!(
                "warn: manifest secrets not provided locally (pass --env): {}",
                missing
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    let mut dsns: HashMap<String, String> = HashMap::new();
    for d in &args.dsn {
        let (k, v) = d
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("--dsn expects name=DSN, got '{d}'"))?;
        dsns.insert(k.to_string(), v.to_string());
    }
    for name in &manifest.capabilities.sql {
        if !dsns.contains_key(name) {
            println!("warn: sql connection '{name}' has no --dsn — opening it will fail");
        }
    }

    let runtime = Runtime::new(Duration::from_secs(30))?;
    // Same policy as the platform: the manifest allowlist, unless --http-out
    // explicitly widens it to all hosts for local experimentation.
    let http_out = if args.http_out {
        HttpOut::Allowed(HostFilter::all())
    } else {
        manifest.http_out_policy()
    };

    let addr = SocketAddr::from(([127, 0, 0, 1], args.port));
    let listener = TcpListener::bind(addr).await?;
    println!(
        "==> serving http://{addr}  (fn: /_fn/*, ui: {})  — Ctrl+C to stop",
        match &dist {
            Some(d) => d.display().to_string(),
            None => "none".into(),
        }
    );

    let dir = Arc::new(dir);
    let runtime = Arc::new(runtime);
    loop {
        let (stream, _) = listener.accept().await?;
        let dir = dir.clone();
        let dist = dist.clone();
        let env = env.clone();
        let dsns = dsns.clone();
        let logs = logs.clone();
        let sql_allowed = manifest.capabilities.sql.clone();
        let kv_allowed = manifest.capabilities.kv.clone();
        let blob_allowed = manifest.capabilities.blobstore.clone();
        let app_name = manifest.name.clone();
        let data_dir = data_dir.clone();
        let http_out = http_out.clone();
        let runtime = runtime.clone();
        tokio::spawn(async move {
            let service = service_fn(move |req: Request<Incoming>| {
                let runtime = runtime.clone();
                let dir = dir.clone();
                let dist = dist.clone();
                let env = env.clone();
                let dsns = dsns.clone();
                let logs = logs.clone();
                let sql_allowed = sql_allowed.clone();
                let kv_allowed = kv_allowed.clone();
                let blob_allowed = blob_allowed.clone();
                let app_name = app_name.clone();
                let data_dir = data_dir.clone();
                let http_out = http_out.clone();
                async move {
                    handle(
                        &runtime,
                        req,
                        &dir,
                        &app_name,
                        &data_dir,
                        env,
                        dsns,
                        sql_allowed,
                        kv_allowed,
                        blob_allowed,
                        http_out,
                        logs,
                        dist,
                    )
                    .await
                }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                .await;
        });
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle(
    runtime: &Runtime,
    req: Request<Incoming>,
    dir: &Path,
    app_name: &str,
    data_dir: &Path,
    env: Vec<(String, String)>,
    dsns: HashMap<String, String>,
    sql_allowed: Vec<String>,
    kv_allowed: Vec<String>,
    blob_allowed: Vec<String>,
    http_out: HttpOut,
    logs: Arc<LogStore>,
    dist: Option<PathBuf>,
) -> Result<Response<FullBody>, hyper::Error> {
    let path = req.uri().path().to_string();
    if path == "/_fn" || path.starts_with("/_fn/") {
        let (parts, body) = req.into_parts();
        let bytes = match body.collect().await {
            Ok(c) => c.to_bytes(),
            Err(e) => return Ok(err(StatusCode::BAD_REQUEST, &e.to_string())),
        };
        let mut hreq = hyper::Request::builder()
            .method(parts.method)
            .uri(parts.uri);
        for (k, v) in &parts.headers {
            hreq = hreq.header(k, v);
        }
        // dev stand-in for the platform user (prod adds x-user-* from OIDC)
        hreq = hreq
            .header("x-user-id", "dev")
            .header("x-user-name", "dev")
            .header("x-user-email", "dev@localhost");
        let req = match hreq.body(Full::new(bytes)) {
            Ok(r) => r,
            Err(e) => return Ok(err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())),
        };

        let mut log_rx = logs.subscribe(app_name);
        let caps = HostCaps {
            artifact_pin: None,
            completion: None,
            http_out,
            sql_allowed,
            sql_dsns: dsns,
            kv_allowed,
            blob_allowed,
            // re-read per request (cheap yaml), mirrors the controller
            blob_stores: pushit_controller::blob_host::load_blobstores(data_dir)
                .unwrap_or_default(),
            log: LogCtx {
                app: app_name.to_string(),
                logs,
            },
        };
        let res = runtime.call(dir, app_name, data_dir, env, caps, req).await;
        while let Ok(line) = log_rx.try_recv() {
            print!("{line}");
        }
        let result = match res {
            Ok(r) => r,
            Err(e) => return Ok(err(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}"))),
        };
        if !result.stderr.is_empty() {
            eprint!("{}", String::from_utf8_lossy(&result.stderr));
        }
        if !result.stdout.is_empty() {
            print!("{}", String::from_utf8_lossy(&result.stdout));
        }
        return Ok(match result.response {
            Some(r) => buffered(r).await,
            None => err(StatusCode::NOT_FOUND, "handler produced no response"),
        });
    }

    // static, mirrors the controller's serve_static
    let Some(dist) = dist else {
        return Ok(err(StatusCode::NOT_FOUND, "no ui_dist in pushit.yaml"));
    };
    let mut rel = path.trim_start_matches('/').to_string();
    if rel.is_empty() {
        rel = "index.html".into();
    }
    if rel.split('/').any(|seg| seg == "..") {
        return Ok(err(StatusCode::BAD_REQUEST, "bad path"));
    }
    let mut path = dist.join(&rel);
    if path.is_dir() {
        path = path.join("index.html");
    }
    if !path.is_file() {
        // SPA fallback
        path = dist.join("index.html");
        if !path.is_file() {
            return Ok(err(StatusCode::NOT_FOUND, "not found"));
        }
    }
    let body = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => return Ok(err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())),
    };
    let mime = mime_guess::from_path(&path)
        .first_raw()
        .unwrap_or("application/octet-stream")
        .to_string();
    Ok(Response::builder()
        .header(hyper::header::CONTENT_TYPE, mime)
        .header(hyper::header::CACHE_CONTROL, "no-cache")
        .body(Full::new(Bytes::from(body)))
        .unwrap())
}

async fn buffered(
    r: hyper::Response<pushit_controller::wasmtime_wasi_http::body::HyperOutgoingBody>,
) -> Response<FullBody> {
    let (parts, body) = r.into_parts();
    let bytes = match BodyExt::collect(body).await {
        Ok(c) => c.to_bytes(),
        Err(_) => Bytes::new(),
    };
    Response::from_parts(parts, Full::new(bytes))
}

fn err(status: StatusCode, msg: &str) -> Response<FullBody> {
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(format!("{msg}\n"))))
        .unwrap()
}
