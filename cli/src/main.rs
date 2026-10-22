use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use flate2::write::GzEncoder;
use flate2::Compression;
use reqwest::blocking::{multipart, Client, Response};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// Thin client for the PushIt controller API.
#[derive(Parser)]
#[command(name = "pushit", version, about = "Deploy and manage PushIt apps")]
struct Cli {
    /// Controller base URL.
    #[arg(long, global = true, env = "PUSHIT_SERVER", default_value = "http://localhost:9100")]
    server: String,
    /// Bearer token for authenticated API calls.
    #[arg(long, global = true, env = "PUSHIT_TOKEN")]
    token: Option<String>,
    /// Dev mode: send X-Pushit-Dev: 1 instead of requiring a token.
    #[arg(long, global = true, env = "PUSHIT_DEV")]
    dev: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print the PushIt app development guide (for agents and humans).
    Skill,
    /// Run manifest build commands (build.fn / build.ui) and verify artifacts.
    Build(BuildArgs),
    /// Scaffold pushit.yaml + fn/ + ui/ in the current directory.
    Init(InitArgs),
    /// Build the artifact archive and deploy to the controller.
    Deploy(DeployArgs),
    /// Manage app secrets.
    #[command(subcommand)]
    Secret(SecretCmd),
    /// List deployed apps.
    List,
    /// Show app detail and revision history.
    Status { app: String },
    /// Roll back to a previous revision.
    Rollback {
        app: String,
        /// Revision id; omit for the previous revision.
        #[arg(long)]
        revision: Option<String>,
    },
    /// Delete an app (asks for confirmation).
    Delete { app: String },
    /// Tail app logs, or stream them with -f.
    Logs(LogsArgs),
}

#[derive(Args)]
struct InitArgs {
    /// App name for pushit.yaml (default: directory name).
    #[arg(long)]
    name: Option<String>,
}

#[derive(Args)]
struct DeployArgs {
    /// Path to pushit.yaml.
    #[arg(long, default_value = "pushit.yaml")]
    manifest: PathBuf,
    /// Override path to the wasm component.
    #[arg(long)]
    wasm: Option<PathBuf>,
    /// Override path to the static dist/ directory.
    #[arg(long)]
    dist: Option<PathBuf>,
}

#[derive(Subcommand)]
enum SecretCmd {
    /// Set a secret: pushit secret set <app> <KEY>=<value>
    Set { app: String, pair: String },
    /// List secret names (values are never returned).
    Ls { app: String },
    /// Remove a secret.
    Rm { app: String, key: String },
}

#[derive(Args)]
struct LogsArgs {
    app: String,
    /// Stream logs over SSE instead of a one-shot tail.
    #[arg(short = 'f', long)]
    follow: bool,
    /// Number of lines for one-shot tail.
    #[arg(long, default_value_t = 200)]
    tail: u32,
}

#[derive(Args)]
struct BuildArgs {
    /// Path to pushit.yaml.
    #[arg(long, default_value = "pushit.yaml")]
    manifest: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = client(&cli)?;
    let server = cli.server.trim_end_matches('/').to_string();
    match cli.cmd {
        Cmd::Skill => {
            print!("{}", include_str!("../../skills/pushit-app/SKILL.md"));
            Ok(())
        }
        Cmd::Build(a) => build(&a),
        Cmd::Init(a) => init(a),
        Cmd::Deploy(a) => deploy(&server, &client, a),
        Cmd::Secret(s) => secret(&server, &client, s),
        Cmd::List => show(client.get(url(&server, "/api/apps")).send()?),
        Cmd::Status { app } => {
            show(client.get(url(&server, &format!("/api/apps/{app}"))).send()?)
        }
        Cmd::Rollback { app, revision } => {
            let body = match revision {
                Some(r) => format!("{{\"revision\":\"{r}\"}}"),
                None => "{}".into(),
            };
            show(client
                .post(url(&server, &format!("/api/apps/{app}/rollback")))
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body)
                .send()?)
        }
        Cmd::Delete { app } => delete(&server, &client, &app),
        Cmd::Logs(a) => logs(&server, &client, a),
    }
}

fn url(server: &str, path: &str) -> String {
    format!("{server}{path}")
}

fn client(cli: &Cli) -> Result<Client> {
    let mut headers = HeaderMap::new();
    if let Some(t) = &cli.token {
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {t}")).context("invalid token")?,
        );
    }
    if cli.dev {
        headers.insert("X-Pushit-Dev", HeaderValue::from_static("1"));
    }
    Ok(Client::builder().default_headers(headers).build()?)
}

/// Print non-2xx body and exit 1, otherwise return the response.
fn check(resp: Response) -> Result<Response> {
    if resp.status().is_success() {
        Ok(resp)
    } else {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        eprintln!("error: {status}\n{body}");
        std::process::exit(1);
    }
}

fn show(resp: Response) -> Result<()> {
    let body = check(resp)?.text()?;
    println!("{body}");
    Ok(())
}

fn build(args: &BuildArgs) -> Result<()> {
    let manifest_text = fs::read_to_string(&args.manifest)
        .with_context(|| format!("read {}", args.manifest.display()))?;
    let manifest: serde_yaml::Value =
        serde_yaml::from_str(&manifest_text).context("parse pushit.yaml")?;
    let base = args.manifest.parent().unwrap_or_else(|| Path::new("."));

    let mut ran_any = false;
    for key in ["fn", "ui"] {
        let Some(cmd) = manifest
            .get("build")
            .and_then(|b| b.get(key))
            .and_then(|v| v.as_str())
        else {
            continue;
        };
        ran_any = true;
        println!("==> build.{key}: {cmd}");
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(base)
            .status()
            .with_context(|| format!("spawn build.{key}"))?;
        if !status.success() {
            bail!("build.{key} failed (exit {status}) — see output above");
        }
    }
    if !ran_any {
        println!("no build.fn/build.ui commands in pushit.yaml — artifacts must exist already");
    }

    // verify artifacts: wasm via entry/find_wasm, dist via build.ui_dist
    let wasm = manifest_path(&manifest, "entry", base)
        .filter(|p| p.is_file())
        .or_else(|| find_wasm(base).ok())
        .ok_or_else(|| anyhow!("build done but no wasm found — check `entry` in pushit.yaml"))?;
    println!("ok wasm: {}", wasm.display());
    if let Some(d) = manifest_path(&manifest, "ui_dist", base) {
        if d.is_dir() {
            println!("ok dist: {}", d.display());
        } else {
            println!("warn: ui_dist {} not found — app will deploy without UI", d.display());
        }
    }
    Ok(())
}

fn deploy(server: &str, client: &Client, args: DeployArgs) -> Result<()> {
    let manifest_text =
        fs::read_to_string(&args.manifest).with_context(|| format!("read {}", args.manifest.display()))?;
    let manifest: serde_yaml::Value = serde_yaml::from_str(&manifest_text).context("parse pushit.yaml")?;
    let name = manifest
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("pushit.yaml: missing `name`"))?;
    let base = args.manifest.parent().unwrap_or_else(|| Path::new("."));

    let wasm = match args.wasm.or_else(|| manifest_path(&manifest, "entry", base)) {
        Some(p) => p,
        None => find_wasm(base)?,
    };
    let dist = args
        .dist
        .or_else(|| manifest_path(&manifest, "ui_dist", base))
        .or_else(|| {
            let d = base.join("dist");
            d.is_dir().then_some(d)
        });

    let archive = build_archive(&wasm, dist.as_deref())?;
    let form = multipart::Form::new()
        .part("manifest", multipart::Part::text(manifest_text).file_name("pushit.yaml"))
        .part(
            "archive",
            multipart::Part::bytes(archive)
                .file_name("archive.tar.gz")
                .mime_str("application/gzip")?,
        );
    let resp = client
        .post(url(server, &format!("/api/apps/{name}/deploy")))
        .multipart(form)
        .send()?;
    println!("{}", check(resp)?.text()?);
    Ok(())
}

fn manifest_path(manifest: &serde_yaml::Value, key: &str, base: &Path) -> Option<PathBuf> {
    let v = manifest.get(key).and_then(|v| v.as_str());
    // ui_dist lives under build: per the app contract; entry is top-level.
    let v = v.or_else(|| {
        manifest
            .get("build")
            .and_then(|b| b.get(key))
            .and_then(|v| v.as_str())
    })?;
    Some(base.join(v))
}

fn find_wasm(base: &Path) -> Result<PathBuf> {
    for triple in ["wasm32-wasip2", "wasm32-wasip1"] {
        let dir = base.join(format!("fn/target/{triple}/release"));
        if let Ok(entries) = fs::read_dir(&dir) {
            let mut files: Vec<PathBuf> = entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "wasm"))
                .collect();
            files.sort();
            if let Some(p) = files.into_iter().next() {
                return Ok(p);
            }
        }
    }
    bail!("no wasm found under fn/target/wasm32-wasip*/release — build with `cargo component build --release` or pass --wasm");
}

fn build_archive(wasm: &Path, dist: Option<&Path>) -> Result<Vec<u8>> {
    let enc = GzEncoder::new(Vec::new(), Compression::default());
    let mut tar = tar::Builder::new(enc);
    tar.append_path_with_name(wasm, "app.wasm")
        .with_context(|| format!("read {}", wasm.display()))?;
    if let Some(d) = dist {
        if d.is_dir() {
            tar.append_dir_all("dist", d)
                .with_context(|| format!("read {}", d.display()))?;
        }
    }
    let enc = tar.into_inner()?;
    Ok(enc.finish()?)
}

fn secret(server: &str, client: &Client, cmd: SecretCmd) -> Result<()> {
    match cmd {
        SecretCmd::Set { app, pair } => {
            let (key, value) = pair
                .split_once('=')
                .ok_or_else(|| anyhow!("expected <KEY>=<value>"))?;
            let resp = client
                .put(url(server, &format!("/api/apps/{app}/secrets/{key}")))
                .header(reqwest::header::CONTENT_TYPE, "text/plain")
                .body(value.to_string())
                .send()?;
            check(resp)?;
            println!("set {key} for {app}");
        }
        SecretCmd::Ls { app } => {
            let resp = client.get(url(server, &format!("/api/apps/{app}/secrets"))).send()?;
            println!("{}", check(resp)?.text()?);
        }
        SecretCmd::Rm { app, key } => {
            let resp = client
                .delete(url(server, &format!("/api/apps/{app}/secrets/{key}")))
                .send()?;
            check(resp)?;
            println!("removed {key} from {app}");
        }
    }
    Ok(())
}

fn delete(server: &str, client: &Client, app: &str) -> Result<()> {
    eprint!("delete app '{app}'? [y/N] ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    if !matches!(line.trim().to_lowercase().as_str(), "y" | "yes") {
        println!("aborted");
        return Ok(());
    }
    let resp = client.delete(url(server, &format!("/api/apps/{app}"))).send()?;
    check(resp)?;
    println!("deleted {app}");
    Ok(())
}

fn logs(server: &str, client: &Client, args: LogsArgs) -> Result<()> {
    if args.follow {
        let resp = client
            .get(url(server, &format!("/api/apps/{}/logs/stream", args.app)))
            .send()?;
        let resp = check(resp)?;
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        for line in BufReader::new(resp).lines() {
            let line = line?;
            if let Some(data) = line.strip_prefix("data:") {
                writeln!(out, "{}", data.trim_start())?;
                out.flush()?;
            }
        }
    } else {
        let resp = client
            .get(url(server, &format!("/api/apps/{}/logs?tail={}", args.app, args.tail)))
            .send()?;
        let body = check(resp)?.text()?;
        print!("{body}");
    }
    Ok(())
}

const INIT_MANIFEST: &str = r#"name: {name}
entry: fn/target/wasm32-wasip2/release/{crate_name}.wasm
env:
  GREETING: hello from env
secrets: []
build:
  fn: cd fn && cargo component build --release --target wasm32-wasip2
  ui_dist: ui
"#;

const INIT_WIT: &str = r#"package pushit:{name};

world app {
    import wasi:cli/environment@0.2.0;
    export wasi:http/incoming-handler@0.2.0;
}
"#;

const INIT_FN_TOML: &str = r#"[package]
name = "{crate_name}"
version = "0.1.0"
edition = "2021"

[dependencies]
wit-bindgen-rt = "0.44"

[lib]
crate-type = ["cdylib"]

[package.metadata.component]
package = "pushit:{name}"

[package.metadata.component.target.dependencies]
"wasi:http" = "=0.2.0"
"wasi:cli" = "=0.2.0"
"#;

const INIT_LIB_RS: &str = r#"#[allow(warnings)]
mod bindings;

use bindings::exports::wasi::http::incoming_handler::Guest;
use bindings::wasi::http::types::{Fields, OutgoingBody, OutgoingResponse, ResponseOutparam};

struct Component;

impl Guest for Component {
    fn handle(_request: bindings::wasi::http::types::IncomingRequest, out: ResponseOutparam) {
        let headers = Fields::new();
        let _ = headers.set(&"content-type".to_string(), &[b"application/json".to_vec()]);
        let resp = OutgoingResponse::new(headers);
        let _ = resp.set_status_code(200);
        let body = resp.body().unwrap();
        ResponseOutparam::set(out, Ok(resp));
        let stream = body.write().unwrap();
        let _ = stream.blocking_write_and_flush(b"{\"msg\":\"hello\"}");
        drop(stream);
        let _ = OutgoingBody::finish(body, None);
    }
}

bindings::export!(Component with_types_in bindings);
"#;

const INIT_INDEX_HTML: &str = r#"<!doctype html>
<meta charset="utf-8">
<title>{name}</title>
<script src="/config.js"></script>
<h1>{name}</h1>
"#;

fn init(args: InitArgs) -> Result<()> {
    let dir = std::env::current_dir()?;
    let name = args.name.unwrap_or_else(|| {
        dir.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("my-app")
            .to_string()
    });
    let crate_name = name.replace('-', "_");

    let write_new = |rel: &str, content: String| -> Result<()> {
        let path = dir.join(rel);
        if path.exists() {
            bail!("{rel} already exists");
        }
        if let Some(p) = path.parent() {
            fs::create_dir_all(p)?;
        }
        fs::write(&path, content)?;
        println!("created {rel}");
        Ok(())
    };

    write_new(
        "pushit.yaml",
        INIT_MANIFEST.replace("{name}", &name).replace("{crate_name}", &crate_name),
    )?;
    write_new("fn/Cargo.toml", INIT_FN_TOML.replace("{crate_name}", &crate_name).replace("{name}", &name))?;
    write_new("fn/wit/world.wit", INIT_WIT.replace("{name}", &name))?;
    write_new("fn/src/lib.rs", INIT_LIB_RS.to_string())?;
    write_new("ui/index.html", INIT_INDEX_HTML.replace("{name}", &name))?;
    write_new(
        "fn/.cargo/config.toml",
        "[build]\ntarget = \"wasm32-wasip2\"\n".to_string(),
    )?;
    println!("\nnext: pushit build && pushit --dev deploy");
    Ok(())
}
