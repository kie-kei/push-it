use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use flate2::write::GzEncoder;
use flate2::Compression;
use reqwest::blocking::{multipart, Client, Response};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

mod dev;
mod profiles;

/// Thin client for the PushIt controller API.
#[derive(Parser)]
#[command(name = "pushit", version, about = "Deploy and manage PushIt apps")]
struct Cli {
    /// Controller base URL (overrides the active profile's server).
    #[arg(long, global = true, env = "PUSHIT_SERVER")]
    server: Option<String>,
    /// Bearer token for authenticated API calls.
    #[arg(long, global = true, env = "PUSHIT_TOKEN")]
    token: Option<String>,
    /// Use this profile's server/token without switching the active one.
    #[arg(long, global = true, env = "PUSHIT_PROFILE")]
    profile: Option<String>,
    /// Dev mode: send X-Pushit-Dev: 1 instead of requiring a token.
    #[arg(long, global = true)]
    dev: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args)]
struct SkillInstallArgs {
    /// Install into ./.agents/skills/pushit instead of the home directory.
    #[arg(long)]
    project: bool,
}

#[derive(Subcommand)]
enum ProfileCmd {
    /// List profiles (active one marked with *).
    List,
    /// Show one profile's settings.
    Show { name: String },
    /// Set the active profile.
    Use { name: String },
    /// Create a profile.
    Add {
        name: String,
        /// Controller base URL for this instance.
        #[arg(long)]
        server: String,
    },
    /// Delete a profile.
    Remove { name: String },
}

#[derive(Subcommand)]
enum SkillCmd {
    /// Install the skill for coding agents (~/.agents/skills/pushit, or --project).
    Install(SkillInstallArgs),
}

#[derive(Subcommand)]
enum Cmd {
    /// Compare a root manifest + app repos against live state (no changes).
    Plan {
        /// Path to the root pushit.yaml with an `apps:` section.
        #[arg(long, default_value = "pushit.yaml")]
        manifest: PathBuf,
    },
    /// Print the PushIt app development guide, or install it for agents.
    Skill {
        #[command(subcommand)]
        cmd: Option<SkillCmd>,
    },
    /// Authenticate via OIDC browser (~1 year) or standalone owner password (30 days).
    Login(LoginArgs),
    /// Manage the standalone owner and revocable API tokens.
    Auth {
        #[command(subcommand)]
        cmd: AuthCmd,
    },
    /// Manage connection profiles (per-instance server + token).
    Profile {
        #[command(subcommand)]
        cmd: ProfileCmd,
    },
    /// Run manifest build commands (build.fn / build.ui) and verify artifacts.
    Build(BuildArgs),
    /// Run the app on a local standalone runtime (no controller needed).
    Dev(dev::DevArgs),
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
    /// Manage immutable app versions (deploy creates and activates a version).
    #[command(subcommand)]
    Version(VersionCmd),
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
struct LoginArgs {
    /// Use standalone owner authentication (never opens a browser or puts tokens in URLs).
    #[arg(long)]
    local: bool,
    /// Use standalone owner authentication over HTTPS through a reverse proxy.
    #[arg(long, conflicts_with = "local")]
    builtin: bool,
    /// Read one password line from stdin instead of the hidden terminal prompt.
    #[arg(long)]
    password_stdin: bool,
}

#[derive(Subcommand)]
enum AuthCmd {
    /// Initialize the singleton owner on the controller computer (cannot replace it).
    Setup {
        #[arg(long, env = "PUSHIT_DATA", default_value = "./pushit-data")]
        data_dir: PathBuf,
        #[arg(long)]
        password_stdin: bool,
    },
    /// List unexpired API token IDs and expiry times (never their values).
    Tokens,
    /// Revoke an API token by ID, immediately and across controller restarts.
    Revoke { id: String },
}

#[derive(Subcommand)]
enum VersionCmd {
    /// List versions, newest first (revision IDs are version IDs).
    List { app: String },
    /// Atomically activate an existing version, restoring code + configuration.
    Activate { app: String, version: String },
    /// Stop serving this app; keep all versions, secrets and data.
    Deactivate { app: String },
    /// Permanently delete one inactive version, never app data or secrets.
    Delete {
        app: String,
        version: String,
        /// Skip the interactive deletion confirmation.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Args)]
struct InitArgs {
    /// App name for pushit.yaml (default: directory name).
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
    /// Platform-wide secrets shared by apps (bind via root manifest `sql: platform:<name>`).
    Global {
        #[command(subcommand)]
        cmd: GlobalCmd,
    },
}

#[derive(clap::Subcommand)]
enum GlobalCmd {
    /// Set a platform-wide secret: pushit secret global set <KEY>=<value>
    Set { pair: String },
    /// List platform-wide secret names.
    Ls,
    /// Remove a platform-wide secret.
    Rm { key: String },
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
    let store = profiles::load();

    // resolution: --profile <name> → explicit target (error if missing);
    // otherwise the active profile; --server/--token still win per-field.
    let target = match &cli.profile {
        Some(name) => Some((name.clone(), store.get(name)?.clone())),
        None => store
            .active
            .as_ref()
            .and_then(|n| store.get(n).ok().map(|e| (n.clone(), e.clone()))),
    };
    let server = cli
        .server
        .clone()
        .or_else(|| target.as_ref().map(|(_, e)| e.server.clone()))
        .unwrap_or_else(|| "http://localhost:9100".into())
        .trim_end_matches('/')
        .to_string();
    let token = cli
        .token
        .clone()
        .or_else(|| {
            target
                .as_ref()
                .map(|(_, e)| e.token.clone())
                .filter(|t| !t.is_empty())
        })
        .or_else(read_saved_token);
    let dev = cli.dev
        || std::env::var("PUSHIT_DEV")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
    let client = client_for(&server, dev, token);
    match cli.cmd {
        Cmd::Skill { cmd } => match cmd {
            None => {
                print!("{}", include_str!("../../skills/pushit-app/SKILL.md"));
                Ok(())
            }
            Some(SkillCmd::Install(a)) => skill_install(&a),
        },
        Cmd::Login(args) => {
            let target_profile = cli.profile.clone().or_else(|| store.active.clone());
            if args.local {
                local_server(&server)?;
            }
            if args.builtin {
                builtin_server(&server)?;
            }
            let local = args.local || args.builtin || args.password_stdin || local_mode(&server);
            if local {
                local_login(&server, target_profile.as_deref(), args.password_stdin)
            } else {
                login(&server, target_profile.as_deref())
            }
        }
        Cmd::Auth { cmd } => match cmd {
            AuthCmd::Setup {
                data_dir,
                password_stdin,
            } => {
                let password = read_password(password_stdin)?;
                if !password_stdin {
                    anyhow::ensure!(
                        rpassword::prompt_password("Confirm owner password: ")? == password,
                        "passwords do not match"
                    );
                }
                pushit_controller::local_auth::LocalAuth::setup(&data_dir, &password)?;
                println!("Owner initialized. Start the controller with PUSHIT_AUTH_MODE=local or builtin and this PUSHIT_DATA directory.");
                Ok(())
            }
            AuthCmd::Tokens => show(client.get(url(&server, "/api/tokens")).send()?),
            AuthCmd::Revoke { id } => {
                anyhow::ensure!(
                    id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
                    "invalid token ID"
                );
                check(
                    client
                        .delete(url(&server, &format!("/api/tokens/{id}")))
                        .send()?,
                )?;
                println!("Token revoked.");
                Ok(())
            }
        },
        Cmd::Profile { cmd } => profile_cmd(cmd),
        Cmd::Plan { manifest } => plan_root(
            &server,
            &client,
            &DeployArgs {
                manifest,
                wasm: None,
                dist: None,
            },
        ),
        Cmd::Build(a) => build(&a),
        Cmd::Dev(a) => dev::run(&a),
        Cmd::Init(a) => init(a),
        Cmd::Deploy(a) => deploy(&server, &client, a),
        Cmd::Secret(s) => secret(&server, &client, s),
        Cmd::List => show(client.get(url(&server, "/api/apps")).send()?),
        Cmd::Status { app } => show(
            client
                .get(url(&server, &format!("/api/apps/{app}")))
                .send()?,
        ),
        Cmd::Rollback { app, revision } => {
            let body = match revision {
                Some(r) => serde_json::json!({"revision": r}).to_string(),
                None => "{}".into(),
            };
            show(
                client
                    .post(url(&server, &format!("/api/apps/{app}/rollback")))
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(body)
                    .send()?,
            )
        }
        Cmd::Version(cmd) => version_cmd(&server, &client, cmd),
        Cmd::Delete { app } => delete(&server, &client, &app),
        Cmd::Logs(a) => logs(&server, &client, a),
    }
}

fn version_cmd(server: &str, client: &Client, cmd: VersionCmd) -> Result<()> {
    // Reject path separators rather than interpolating arbitrary paths into API
    // URLs. Version IDs are opaque revision IDs, not Git SHAs.
    let valid = |value: &str| -> Result<()> {
        if value.is_empty()
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        {
            bail!("invalid app/version identifier '{value}'");
        }
        Ok(())
    };
    match cmd {
        VersionCmd::List { app } => {
            valid(&app)?;
            show(
                client
                    .get(url(server, &format!("/api/apps/{app}/versions")))
                    .send()?,
            )
        }
        VersionCmd::Activate { app, version } => {
            valid(&app)?;
            valid(&version)?;
            show(
                client
                    .post(url(
                        server,
                        &format!("/api/apps/{app}/versions/{version}/activate"),
                    ))
                    .send()?,
            )
        }
        VersionCmd::Deactivate { app } => {
            valid(&app)?;
            show(
                client
                    .post(url(server, &format!("/api/apps/{app}/deactivate")))
                    .send()?,
            )
        }
        VersionCmd::Delete { app, version, yes } => {
            valid(&app)?;
            valid(&version)?;
            if !yes {
                eprint!("Permanently delete inactive version {version} of {app}? App data/secrets are kept. [y/N] ");
                std::io::stderr().flush()?;
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;
                if !answer.trim().eq_ignore_ascii_case("y") {
                    println!("Canceled.");
                    return Ok(());
                }
            }
            show(
                client
                    .delete(url(server, &format!("/api/apps/{app}/versions/{version}")))
                    .send()?,
            )
        }
    }
}

fn url(server: &str, path: &str) -> String {
    format!("{server}{path}")
}

fn client_for(server: &str, dev: bool, token: Option<String>) -> Client {
    let mut headers = HeaderMap::new();
    if let Some(t) = token {
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {t}")).unwrap_or_else(|_| {
                eprintln!("error: invalid token");
                std::process::exit(1);
            }),
        );
    }
    if dev {
        headers.insert("X-Pushit-Dev", HeaderValue::from_static("1"));
    }
    let mut builder = Client::builder()
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::none());
    if local_server(server).is_ok() {
        builder = builder
            .no_proxy()
            .resolve_to_addrs(
                "hub.localhost",
                &["127.0.0.1:0".parse().unwrap(), "[::1]:0".parse().unwrap()],
            )
            .redirect(reqwest::redirect::Policy::none());
    }
    builder.build().unwrap()
}

fn saved_token_path() -> Option<PathBuf> {
    let cfg = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .ok()
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(cfg.join("pushit").join("token"))
}

fn read_saved_token() -> Option<String> {
    std::fs::read_to_string(saved_token_path()?)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Print non-2xx body and exit 1, otherwise return the response.
fn check(resp: Response) -> Result<Response> {
    if resp.status().is_success() {
        Ok(resp)
    } else {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        eprintln!("error: {status}\n{body}");
        if status == 401 {
            eprintln!("hint: run `pushit login` to get a long-lived token, or check --token/--dev");
        }
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
    let base = manifest_base(&args.manifest);
    run_build_steps(&manifest, &base)
}

/// Run `build.fn`/`build.ui` from the manifest + verify artifacts.
fn run_build_steps(manifest: &serde_yaml::Value, base: &Path) -> Result<()> {
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
            println!(
                "warn: ui_dist {} not found — app will deploy without UI",
                d.display()
            );
        }
    }
    Ok(())
}

fn deploy(server: &str, client: &Client, args: DeployArgs) -> Result<()> {
    let manifest_text = fs::read_to_string(&args.manifest)
        .with_context(|| format!("read {}", args.manifest.display()))?;
    let manifest: serde_yaml::Value =
        serde_yaml::from_str(&manifest_text).context("parse pushit.yaml")?;

    // Root manifest: `apps:` maps name → {repo, ref?, manifest?}.
    // Each app is cloned/pulled next to the root manifest and built+deployed
    // from its own pushit.yaml (build runs locally, per app).
    if manifest.get("apps").is_some() {
        return deploy_root(server, client, &args, &manifest);
    }

    deploy_one(server, client, args, manifest_text, &manifest)
}

fn deploy_root(
    server: &str,
    client: &Client,
    args: &DeployArgs,
    manifest: &serde_yaml::Value,
) -> Result<()> {
    let apps = manifest
        .get("apps")
        .and_then(|v| v.as_mapping())
        .ok_or_else(|| {
            anyhow!("root manifest: `apps:` must be a mapping of name → {{repo, ...}}")
        })?;
    let base = manifest_base(&args.manifest);
    let cache = base.join(".pushit-cache");
    fs::create_dir_all(&cache)?;

    let mut failed = Vec::new();
    for (key, cfg) in apps {
        let key = key
            .as_str()
            .ok_or_else(|| anyhow!("apps: keys must be strings"))?;
        let repo = cfg
            .get("repo")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("apps.{key}: missing `repo`"))?;
        let git_ref = cfg.get("ref").and_then(|v| v.as_str());
        let inner = cfg
            .get("manifest")
            .and_then(|v| v.as_str())
            .unwrap_or("pushit.yaml");

        let dir = cache.join(key);
        println!("==> {key}: sync repository ({})", git_ref.unwrap_or("HEAD"));
        if let Err(e) = sync_repo(repo, git_ref, &dir) {
            eprintln!("FAIL {key}: sync: {e:#}");
            failed.push(key.to_string());
            continue;
        }
        let app_args = DeployArgs {
            manifest: dir.join(inner),
            wasm: None, // per-app overrides don't make sense from the root manifest
            dist: None,
        };
        let manifest_text = match fs::read_to_string(&app_args.manifest) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("FAIL {key}: {}: {e}", app_args.manifest.display());
                failed.push(key.to_string());
                continue;
            }
        };
        let m: serde_yaml::Value = match serde_yaml::from_str(&manifest_text) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("FAIL {key}: parse {}: {e}", app_args.manifest.display());
                failed.push(key.to_string());
                continue;
            }
        };
        // Runtime env is a deploy-time decision: required here, replaces the
        // repo manifest's env wholesale (config lives with the deployer, not the code).
        let env = match cfg.get("env") {
            Some(v) if v.as_mapping().is_some() => v.clone(),
            _ => {
                eprintln!("FAIL {key}: explicit `env:` required in root manifest (use `env: {{}}` for none)");
                failed.push(key.to_string());
                continue;
            }
        };
        let mut m = m;
        m["env"] = env;
        // capabilities.sql grants: bind app-local logical names to sources
        // (secret:<key> | platform:<global>) — deployer knowledge, not repo.
        if let Some(bindings) = cfg.get("sql") {
            if bindings.as_mapping().is_some() {
                m["sql_bindings"] = bindings.clone();
            } else {
                eprintln!("FAIL {key}: sql must be a mapping of connection → secret:<key> | platform:<global>");
                failed.push(key.to_string());
                continue;
            }
        }
        let manifest_text = serde_yaml::to_string(&m).context("re-serialize manifest")?;
        println!("==> {key}: build + deploy");
        // build locally (never on the server) from the app's own manifest
        let app_base = manifest_base(&app_args.manifest);
        if let Err(e) = run_build_steps(&m, app_base) {
            eprintln!("FAIL {key}: {e:#}");
            failed.push(key.to_string());
            continue;
        }
        match deploy_one(server, client, app_args, manifest_text, &m) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("FAIL {key}: {e:#}");
                failed.push(key.to_string());
            }
        }
    }
    if failed.is_empty() {
        println!("all apps deployed");
        Ok(())
    } else {
        bail!("{} app(s) failed: {}", failed.len(), failed.join(", "))
    }
}

/// Clone or update `repo` at `dir`; shallow, `git_ref` = branch or tag.
fn sync_repo(repo: &str, git_ref: Option<&str>, dir: &Path) -> Result<()> {
    // Git stores local clone origins as absolute paths. Resolve only plain
    // local paths, not URLs or scp-style remotes, for cold/warm consistency.
    let local_repo = if !repo.contains(':') {
        fs::canonicalize(repo).ok()
    } else {
        None
    };
    let repo = match local_repo.as_ref() {
        Some(path) => path.to_str().context("repository path is not UTF-8")?,
        None => repo,
    };
    // Git diagnostics may contain credentials embedded in repository URLs.
    let run = |mut cmd: std::process::Command| -> Result<std::process::Output> {
        let out = cmd.output().context("spawn git")?;
        if !out.status.success() {
            bail!(
                "git repository sync failed (exit {}); check repository access and branch/tag",
                out.status
            );
        }
        Ok(out)
    };
    if let Some(r) = git_ref {
        if r.starts_with('-') {
            bail!("invalid branch/tag name: must not start with a dash");
        }
        let mut check = std::process::Command::new("git");
        check.arg("check-ref-format").arg(format!("refs/heads/{r}"));
        run(check).context("invalid branch/tag name")?;
    }
    if dir.join(".git").exists() {
        // Compare raw configured URLs exactly: guessing equivalent identities can
        // silently use the wrong repository. Never include either URL in errors.
        let mut origin = std::process::Command::new("git");
        origin
            .args(["-C"])
            .arg(dir)
            .args(["config", "--get-all", "remote.origin.url"]);
        let out = run(origin).context("read cached repository origin")?;
        let configured = String::from_utf8(out.stdout).context("invalid cached origin encoding")?;
        if configured.strip_suffix('\n') != Some(repo) {
            bail!("cached repository origin does not exactly match manifest repo; review and move the app cache aside before retrying (cache left unchanged)");
        }
        // `clone --branch` prefers a branch over a tag of the same name. Use
        // fully qualified fetch refs to preserve that choice on a warm cache.
        let selected_ref = if let Some(r) = git_ref {
            let branch = format!("refs/heads/{r}");
            let mut heads = std::process::Command::new("git");
            heads
                .arg("-C")
                .arg(dir)
                .args(["ls-remote", "--exit-code", "--heads", "origin"])
                .arg(&branch);
            let out = heads.output().context("resolve repository branch")?;
            if out.status.success() {
                branch
            } else if out.status.code() == Some(2) {
                format!("refs/tags/{r}")
            } else {
                bail!("could not resolve repository branch; check repository access");
            }
        } else {
            "HEAD".to_string()
        };
        let mut fetch = std::process::Command::new("git");
        fetch
            .arg("-C")
            .arg(dir)
            .args(["fetch", "--depth", "1", "origin"])
            .arg(selected_ref);
        run(fetch)?;
        let mut reset = std::process::Command::new("git");
        reset
            .arg("-C")
            .arg(dir)
            .args(["reset", "--hard", "FETCH_HEAD"]);
        run(reset)?;
        Ok(())
    } else {
        let mut clone = std::process::Command::new("git");
        clone.args(["clone", "--depth", "1"]);
        if let Some(r) = git_ref {
            clone.arg("--branch").arg(r);
        }
        clone.arg("--").arg(repo).arg(dir);
        run(clone)?;
        Ok(())
    }
}

/// `pushit plan`: diff root manifest (+ synced repos) against live state.
/// Read-only: create / env-drift / missing-secrets / orphan detection.
fn plan_root(server: &str, client: &Client, args: &DeployArgs) -> Result<()> {
    let manifest_text = fs::read_to_string(&args.manifest)
        .with_context(|| format!("read {}", args.manifest.display()))?;
    let manifest: serde_yaml::Value =
        serde_yaml::from_str(&manifest_text).context("parse pushit.yaml")?;
    let apps = manifest
        .get("apps")
        .and_then(|v| v.as_mapping())
        .ok_or_else(|| anyhow!("plan: manifest must be a root manifest with an `apps:` section"))?;
    let base = manifest_base(&args.manifest);
    let cache = base.join(".pushit-cache");
    fs::create_dir_all(&cache)?;

    let live: serde_json::Value = client
        .get(url(server, "/api/apps"))
        .send()?
        .error_for_status()?
        .json()?;
    let live_names: Vec<String> = live
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get("name").and_then(|v| v.as_str()))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();

    // orphan detection runs after the loop: compare against real app names
    // (root keys are cache dir names and may differ from app names)

    let mut drift = 0usize;
    let mut expected: Vec<String> = Vec::new();
    for (key, cfg) in apps {
        let key = key
            .as_str()
            .ok_or_else(|| anyhow!("apps: keys must be strings"))?;
        let repo = cfg
            .get("repo")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("apps.{key}: missing `repo`"))?;
        let git_ref = cfg.get("ref").and_then(|v| v.as_str());
        let inner = cfg
            .get("manifest")
            .and_then(|v| v.as_str())
            .unwrap_or("pushit.yaml");
        let dir = cache.join(key);
        print!("{key}: ");
        if let Err(e) = sync_repo(repo, git_ref, &dir) {
            println!("ERROR sync: {e:#}");
            drift += 1;
            continue;
        }
        let m: serde_yaml::Value = match fs::read_to_string(dir.join(inner))
            .with_context(|| format!("read {}/{}", key, inner))
            .and_then(|t| serde_yaml::from_str(&t).context("parse"))
        {
            Ok(m) => m,
            Err(e) => {
                println!("ERROR manifest: {e:#}");
                drift += 1;
                continue;
            }
        };
        let app_name = m
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("apps.{key}: inner manifest missing `name`"))?;

        // env: mandatory at deploy time, same rule as deploy
        let env = match cfg.get("env") {
            Some(v) if v.as_mapping().is_some() => v,
            _ => {
                println!("ERROR explicit `env:` required (deploy would fail)");
                drift += 1;
                continue;
            }
        };

        if !live_names.iter().any(|n| n == app_name) {
            println!("CREATE (new app)");
            drift += 1;
            expected.push(app_name.to_string());
            continue;
        }
        expected.push(app_name.to_string());

        // live manifest from controller
        let detail: serde_json::Value = client
            .get(url(server, &format!("/api/apps/{app_name}")))
            .send()?
            .error_for_status()?
            .json()?;
        let live_manifest: serde_yaml::Value = serde_yaml::from_str(
            detail
                .get("manifest")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        )?;

        // env diff (root env is authoritative)
        let mut changes: Vec<String> = Vec::new();
        if let Some(live_env) = live_manifest.get("env").and_then(|v| v.as_mapping()) {
            for (k, v) in env.as_mapping().unwrap() {
                if live_env.get(k) != Some(v) {
                    changes.push(format!(
                        "env.{} = {:?}",
                        k.as_str().unwrap_or("?"),
                        v.as_str().unwrap_or("?")
                    ));
                }
            }
            for k in live_env.keys() {
                if env.as_mapping().unwrap().get(k).is_none() {
                    changes.push(format!("env.{} removed", k.as_str().unwrap_or("?")));
                }
            }
        }

        // declared secrets vs values present on platform
        let wanted: Vec<&str> = m
            .get("secrets")
            .and_then(|v| v.as_sequence())
            .map(|s| s.iter().filter_map(|x| x.as_str()).collect())
            .unwrap_or_default();
        let have: Vec<String> = client
            .get(url(server, &format!("/api/apps/{app_name}/secrets")))
            .send()?
            .error_for_status()?
            .json::<serde_json::Value>()
            .ok()
            .and_then(|j| {
                j.get("keys").and_then(|k| k.as_array()).map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
            })
            .unwrap_or_default();
        for s in &wanted {
            if !have.iter().any(|h| h == s) {
                changes.push(format!(
                    "secret {s} missing (pushit secret set {app_name} {s}=…)"
                ));
            }
        }

        // sql bindings (root manifest is authoritative)
        if let Some(bindings) = cfg.get("sql").filter(|v| v.as_mapping().is_some()) {
            let live_bindings = live_manifest
                .get("sql_bindings")
                .and_then(|v| v.as_mapping())
                .cloned()
                .unwrap_or_default();
            for (k, v) in bindings.as_mapping().unwrap() {
                if live_bindings.get(k) != Some(v) {
                    changes.push(format!(
                        "sql.{} = {:?}",
                        k.as_str().unwrap_or("?"),
                        v.as_str().unwrap_or("?")
                    ));
                }
            }
        }

        if changes.is_empty() {
            println!("up to date (config; code changes deploy as a new revision)");
        } else {
            drift += 1;
            println!("DRIFT:");
            for c in changes {
                println!("    {c}");
            }
        }
    }
    for name in &live_names {
        if !expected.iter().any(|e| e == name) {
            println!("~ {name}: live on platform but NOT in root manifest (orphan — delete manually if unintended)");
        }
    }
    if drift == 0 {
        println!("plan: no drift");
    } else {
        println!("plan: {drift} app(s) with changes/errors");
    }
    Ok(())
}

fn deploy_one(
    server: &str,
    client: &Client,
    args: DeployArgs,
    manifest_text: String,
    manifest: &serde_yaml::Value,
) -> Result<()> {
    let name = manifest
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("pushit.yaml: missing `name`"))?;
    let base = manifest_base(&args.manifest);

    let wasm = match args
        .wasm
        .or_else(|| manifest_path(&manifest, "entry", base))
    {
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
        .part(
            "manifest",
            multipart::Part::text(manifest_text).file_name("pushit.yaml"),
        )
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

/// Base dir for paths in a manifest. `.parent()` of a bare relative
/// filename ("pushit.yaml") is Some("") — chdir("") is ENOENT, so map it
/// to the current directory.
fn manifest_base(manifest: &Path) -> &Path {
    manifest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
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
    const GLOBAL: &str = "/api/secrets/global";
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
            let resp = client
                .get(url(server, &format!("/api/apps/{app}/secrets")))
                .send()?;
            println!("{}", check(resp)?.text()?);
        }
        SecretCmd::Rm { app, key } => {
            let resp = client
                .delete(url(server, &format!("/api/apps/{app}/secrets/{key}")))
                .send()?;
            check(resp)?;
            println!("removed {key} from {app}");
        }
        SecretCmd::Global { cmd } => match cmd {
            GlobalCmd::Set { pair } => {
                let (key, value) = pair
                    .split_once('=')
                    .ok_or_else(|| anyhow!("expected <KEY>=<value>"))?;
                let resp = client
                    .put(url(server, &format!("{GLOBAL}/{key}")))
                    .header(reqwest::header::CONTENT_TYPE, "text/plain")
                    .body(value.to_string())
                    .send()?;
                check(resp)?;
                println!("set global secret {key}");
            }
            GlobalCmd::Ls => {
                let resp = client.get(url(server, GLOBAL)).send()?;
                println!("{}", check(resp)?.text()?);
            }
            GlobalCmd::Rm { key } => {
                let resp = client
                    .delete(url(server, &format!("{GLOBAL}/{key}")))
                    .send()?;
                check(resp)?;
                println!("removed global secret {key}");
            }
        },
    }
    Ok(())
}

const SKILL_MD: &str = include_str!("../../skills/pushit-app/SKILL.md");
const SKILL_REFERENCES: &[(&str, &str)] = &[
    (
        "sql.md",
        include_str!("../../skills/pushit-app/references/sql.md"),
    ),
    (
        "kv.md",
        include_str!("../../skills/pushit-app/references/kv.md"),
    ),
    (
        "blobstore.md",
        include_str!("../../skills/pushit-app/references/blobstore.md"),
    ),
    (
        "logging.md",
        include_str!("../../skills/pushit-app/references/logging.md"),
    ),
];

fn read_password(from_stdin: bool) -> Result<String> {
    use std::io::{IsTerminal, Read};
    let password = if from_stdin {
        let mut bytes = Vec::new();
        std::io::stdin().take(1027).read_to_end(&mut bytes)?;
        if bytes.ends_with(b"\n") {
            bytes.pop();
            if bytes.ends_with(b"\r") {
                bytes.pop();
            }
        }
        String::from_utf8(bytes).context("password must be UTF-8")?
    } else {
        anyhow::ensure!(
            std::io::stdin().is_terminal(),
            "use --password-stdin for noninteractive input"
        );
        rpassword::prompt_password("Owner password: ")?
    };
    anyhow::ensure!(
        !password.trim().is_empty() && password.len() <= 1024 && !password.contains(['\r', '\n']),
        "password must be one nonempty line of at most 1024 bytes"
    );
    Ok(password)
}

fn local_server(server: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(server)?;
    anyhow::ensure!(url.scheme() == "http" && url.host_str() == Some("hub.localhost") && url.username().is_empty() && url.password().is_none() && url.path() == "/" && url.query().is_none() && url.fragment().is_none(), "local login requires http://hub.localhost:<controller-port> (no proxy, TLS or remote server)");
    Ok(url)
}
fn local_client() -> Result<Client> {
    Ok(Client::builder()
        .no_proxy()
        .resolve_to_addrs(
            "hub.localhost",
            &["127.0.0.1:0".parse()?, "[::1]:0".parse()?],
        )
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .build()?)
}
fn builtin_server(server: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(server)?;
    let host = url.host_str().unwrap_or("");
    let base = host.strip_prefix("hub.").unwrap_or("");
    anyhow::ensure!(url.scheme() == "https" && base.contains('.')
        && !base.ends_with(".localhost") && base != "localhost"
        && pushit_controller::config::valid_dns_name(host)
        && url.port_or_known_default().is_some_and(|p| p != 0)
        && url.username().is_empty() && url.password().is_none()
        && url.path() == "/" && url.query().is_none() && url.fragment().is_none()
        && server == url.as_str().trim_end_matches('/'),
        "builtin login requires a canonical https://hub.<base-domain>[:port] origin (no credentials, path or URL parameters)");
    Ok(url)
}
fn owner_client(server: &str) -> Result<Client> {
    if local_server(server).is_ok() {
        return local_client();
    }
    builtin_server(server)?;
    Ok(Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .build()?)
}
fn local_mode(server: &str) -> bool {
    if local_server(server).is_err() && builtin_server(server).is_err() {
        return false;
    }
    owner_client(server)
        .ok()
        .and_then(|c| c.get(format!("{server}/auth/mode")).send().ok())
        .filter(|r| r.status().is_success())
        .and_then(|r| r.json::<serde_json::Value>().ok())
        .is_some_and(|v| v["mode"] == "local" || v["mode"] == "builtin")
}
fn local_login(server: &str, target_profile: Option<&str>, stdin: bool) -> Result<()> {
    let client = owner_client(server)?;
    let password = read_password(stdin)?;
    let response = client
        .post(format!("{server}/auth/local/token"))
        .header("x-pushit-cli", "1")
        .json(&serde_json::json!({"password": password}))
        .send()?;
    anyhow::ensure!(
        response.status().is_success(),
        "owner login rejected (check password, owner setup and login attempt limit)"
    );
    let result: serde_json::Value = response.json()?;
    let token = result["token"].as_str().context("missing token")?;
    if let Some(name) = target_profile {
        let mut store = profiles::load();
        store.set_token(name, token)?;
        profiles::save(&store)?;
        println!("Owner token saved to profile '{name}' (30 days).");
    } else {
        let path = saved_token_path().context("cannot determine config dir (set HOME)")?;
        profiles::write_private(&path, token.as_bytes())?;
        println!("Owner token saved (30 days).");
    }
    println!(
        "Token ID: {} (pushit auth revoke <id> revokes it)",
        result["id"].as_str().unwrap_or("unknown")
    );
    Ok(())
}

/// `pushit login`: open `{server}/auth/device` in a browser, catch the
/// minted long-lived token on a localhost listener, store it for client().
/// The token lands in the target profile when there is one, else in the
/// legacy single-token file.
fn login(server: &str, target_profile: Option<&str>) -> Result<()> {
    use std::io::{Read as _, Write as _};

    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let url = format!("{server}/auth/device?port={port}");
    println!("opening {url}\n(if the browser does not open, visit it manually)");
    let _ = std::process::Command::new("xdg-open").arg(&url).spawn();

    println!("waiting for authorization (2 min)... ");
    listener.set_nonblocking(false)?;
    let (mut stream, _) = listener.accept()?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    // read until headers end; the token arrives as a query param on GET /
    loop {
        if std::time::Instant::now() > deadline {
            bail!("timed out waiting for browser callback");
        }
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e.into()),
        }
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 64 * 1024 {
            bail!("oversized request");
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let path = head.split_whitespace().nth(1).unwrap_or_default();
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or_default();
    let mut token = None;
    let mut user = None;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "token" => token = Some(v.to_string()),
            "user" => user = Some(v.to_string()),
            _ => {}
        }
    }
    let (Some(token), Some(user)) = (token, user) else {
        bail!("callback did not carry a token");
    };
    let _ = stream.write_all(
        b"HTTP/1.1 200 OK\r\ncontent-type: text/html\r\nconnection: close\r\n\r\n\
          <html><body><h3>Token received.</h3>You can close this tab and return to the terminal.</body></html>",
    );

    let path =
        saved_token_path().ok_or_else(|| anyhow!("cannot determine config dir (set HOME)"))?;
    println!("logged in as {user}");
    match target_profile {
        Some(name) => {
            let mut store = profiles::load();
            store.set_token(name, &token)?;
            profiles::save(&store)?;
            println!("token saved to profile '{name}'");
        }
        None => {
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(&path, &token)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            }
            println!("token saved to {}", path.display());
        }
    }
    println!("token valid ~1 year; `pushit login` rotates it");
    Ok(())
}

fn profile_cmd(cmd: ProfileCmd) -> Result<()> {
    let mut store = profiles::load();
    match cmd {
        ProfileCmd::List => {
            if store.profiles.is_empty() {
                println!("no profiles (create one: pushit profile add <name> --server <url>)");
                return Ok(());
            }
            for (name, e) in &store.profiles {
                let active = if store.active.as_deref() == Some(name) {
                    "*"
                } else {
                    " "
                };
                println!(
                    "{active} {name:<16} {}  token:{}",
                    e.server,
                    if e.token.is_empty() { "not set" } else { "set" }
                );
            }
        }
        ProfileCmd::Show { name } => {
            let e = store.get(&name)?;
            println!("name:   {name}");
            println!("server: {}", e.server);
            println!(
                "token:  {}",
                if e.token.is_empty() {
                    "not set (pushit login --profile {name})"
                } else {
                    "set"
                }
            );
            println!("active: {}", store.active.as_deref() == Some(name.as_str()));
        }
        ProfileCmd::Use { name } => {
            store.get(&name)?;
            store.active = Some(name.clone());
            profiles::save(&store)?;
            println!("active profile: {name}");
        }
        ProfileCmd::Add { name, server } => {
            if store.profiles.contains_key(&name) {
                bail!("profile '{name}' already exists (remove it first)");
            }
            store.profiles.insert(
                name.clone(),
                profiles::Entry {
                    server: server.trim_end_matches('/').to_string(),
                    token: String::new(),
                },
            );
            let first = store.active.is_none();
            if first {
                store.active = Some(name.clone());
            }
            profiles::save(&store)?;
            println!(
                "profile '{name}' added{}",
                if first { " and activated" } else { "" }
            );
            if first {
                println!("next: pushit login --profile {name}");
            }
        }
        ProfileCmd::Remove { name } => {
            store.get(&name)?;
            print!("delete profile '{name}'? [y/N] ");
            std::io::Write::flush(&mut std::io::stderr())?;
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            if !matches!(line.trim().to_lowercase().as_str(), "y" | "yes") {
                println!("aborted");
                return Ok(());
            }
            store.profiles.remove(&name);
            if store.active.as_deref() == Some(name.as_str()) {
                store.active = None;
            }
            profiles::save(&store)?;
            println!("removed {name}");
        }
    }
    Ok(())
}

fn skill_install(a: &SkillInstallArgs) -> Result<()> {
    let target = if a.project {
        PathBuf::from(".agents/skills/pushit/SKILL.md")
    } else {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".agents/skills/pushit/SKILL.md"))
            .ok_or_else(|| anyhow!("HOME is not set; use --project"))?
    };
    print!(
        "install pushit skill to {}?{} [y/N] ",
        target.parent().unwrap().display(),
        if target.exists() {
            " (will overwrite SKILL.md and bundled references)"
        } else {
            ""
        }
    );
    std::io::Write::flush(&mut std::io::stderr())?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    if !matches!(line.trim().to_lowercase().as_str(), "y" | "yes") {
        println!("aborted");
        return Ok(());
    }
    if let Some(dir) = target.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(&target, SKILL_MD)?;
    // Embed references too: an installed binary must not need its build checkout.
    let dest = target.parent().unwrap().join("references");
    fs::create_dir_all(&dest)?;
    for (name, content) in SKILL_REFERENCES {
        fs::write(dest.join(name), content)?;
    }
    println!("installed: {}", target.display());
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
    let resp = client
        .delete(url(server, &format!("/api/apps/{app}")))
        .send()?;
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
            .get(url(
                server,
                &format!("/api/apps/{}/logs?tail={}", args.app, args.tail),
            ))
            .send()?;
        let body = check(resp)?.text()?;
        print!("{body}");
    }
    Ok(())
}

// cargo-component builds a WASI Preview 1 core module and adapts it into a
// component in the same target directory; the deploy entry must match that path.
const INIT_MANIFEST: &str = r#"name: {name}
entry: fn/target/wasm32-wasip1/release/{crate_name}.wasm
env:
  GREETING: hello from env
secrets: []
build:
  fn: cd fn && cargo component build --release --target wasm32-wasip1
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
"wasi:http" = { path = "wit/deps/http" }
"wasi:cli" = { path = "wit/deps/cli" }
"wasi:io" = { path = "wit/deps/io" }
"wasi:clocks" = { path = "wit/deps/clocks" }

# Keep generated apps independent of any enclosing Cargo workspace.
[workspace]
"#;

// Reuse the pinned example interfaces without duplicating their source assets.
const INIT_WIT_DEPS: &[(&str, &str)] = &[
    (
        "http/handler.wit",
        include_str!("../../examples/hello/fn/wit/deps/http/handler.wit"),
    ),
    (
        "http/proxy.wit",
        include_str!("../../examples/hello/fn/wit/deps/http/proxy.wit"),
    ),
    (
        "http/types.wit",
        include_str!("../../examples/hello/fn/wit/deps/http/types.wit"),
    ),
    (
        "cli/environment.wit",
        include_str!("../../examples/hello/fn/wit/deps/cli/environment.wit"),
    ),
    (
        "cli/imports.wit",
        include_str!("../../examples/hello/fn/wit/deps/cli/imports.wit"),
    ),
    (
        "io/error.wit",
        include_str!("../../examples/hello/fn/wit/deps/io/error.wit"),
    ),
    (
        "io/poll.wit",
        include_str!("../../examples/hello/fn/wit/deps/io/poll.wit"),
    ),
    (
        "io/streams.wit",
        include_str!("../../examples/hello/fn/wit/deps/io/streams.wit"),
    ),
    (
        "io/world.wit",
        include_str!("../../examples/hello/fn/wit/deps/io/world.wit"),
    ),
    (
        "clocks/monotonic-clock.wit",
        include_str!("../../examples/hello/fn/wit/deps/clocks/monotonic-clock.wit"),
    ),
    (
        "clocks/wall-clock.wit",
        include_str!("../../examples/hello/fn/wit/deps/clocks/wall-clock.wit"),
    ),
    (
        "clocks/world.wit",
        include_str!("../../examples/hello/fn/wit/deps/clocks/world.wit"),
    ),
];
const INIT_WIT_NOTICE: &str = include_str!("../../examples/hello/fn/wit/NOTICE.md");

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
    let has_name = args.name.is_some();
    let name = args.name.unwrap_or_else(|| {
        dir.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("my-app")
            .to_string()
    });
    // README workflow: `pushit init myapp && cd myapp` — scaffold into the
    // named subdirectory; without a name, use the current directory.
    let dir = if has_name {
        let target = dir.join(&name);
        fs::create_dir_all(&target)?;
        target
    } else {
        dir
    };
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
        INIT_MANIFEST
            .replace("{name}", &name)
            .replace("{crate_name}", &crate_name),
    )?;
    write_new(
        "fn/Cargo.toml",
        INIT_FN_TOML
            .replace("{crate_name}", &crate_name)
            .replace("{name}", &name),
    )?;
    write_new("fn/wit/world.wit", INIT_WIT.replace("{name}", &name))?;
    for (path, content) in INIT_WIT_DEPS {
        write_new(&format!("fn/wit/deps/{path}"), (*content).to_string())?;
    }
    write_new("fn/wit/NOTICE.md", INIT_WIT_NOTICE.to_string())?;
    write_new("fn/src/lib.rs", INIT_LIB_RS.to_string())?;
    write_new("ui/index.html", INIT_INDEX_HTML.replace("{name}", &name))?;
    write_new(
        "fn/.cargo/config.toml",
        "[build]\ntarget = \"wasm32-wasip1\"\n".to_string(),
    )?;
    println!("\nnext: pushit build; then deploy using an authorized profile, or use pushit --dev --server http://127.0.0.1:9100 deploy for a local controller");
    Ok(())
}

#[cfg(test)]
mod sync_repo_tests {
    use super::sync_repo;
    use std::{fs, path::Path, process::Command};

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn isolated_local_repositories() {
        let root = std::env::temp_dir().join(format!("pushit-sync-{}", std::process::id()));
        fs::create_dir(&root).unwrap();
        // Isolate all Git configuration without mutating the test runner's environment.
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "sync_repo_tests::local_fixture",
                "--nocapture",
            ])
            .env("PUSHIT_SYNC_TEST_ROOT", &root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_COUNT", "0")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("HOME", &root)
            .output()
            .unwrap();
        fs::remove_dir_all(&root).unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }

    #[test]
    #[ignore = "run through isolated_local_repositories with isolated Git configuration"]
    fn local_fixture() {
        let root = std::path::PathBuf::from(std::env::var_os("PUSHIT_SYNC_TEST_ROOT").unwrap());
        std::env::set_current_dir(&root).unwrap();
        let a = root.join("A");
        let b = root.join("B");
        for repo in [&a, &b] {
            fs::create_dir(repo).unwrap();
            git(repo, &["init", "-b", "main"]);
            git(repo, &["config", "user.email", "fixture@example.invalid"]);
            git(repo, &["config", "user.name", "Fixture"]);
            fs::write(repo.join("content"), "initial").unwrap();
            git(repo, &["add", "."]);
            git(repo, &["commit", "-m", "initial"]);
        }
        let a_url = format!("file://{}", a.display());
        let b_url = format!("file://{}", b.display());
        git(&a, &["tag", "collision"]);
        git(&a, &["tag", "release"]);
        fs::write(a.join("content"), "branch").unwrap();
        git(&a, &["commit", "-am", "branch"]);
        git(&a, &["branch", "collision"]);
        let expected = git(&a, &["rev-parse", "HEAD"]);
        for (name, reference) in [
            ("default", None),
            ("branch", Some("main")),
            ("collision", Some("collision")),
            ("tag", Some("release")),
        ] {
            let cache = root.join(name);
            sync_repo(&a_url, reference, &cache).unwrap();
            let cold = git(&cache, &["rev-parse", "HEAD"]);
            fs::write(cache.join("untracked"), "preserve").unwrap();
            sync_repo(&a_url, reference, &cache).unwrap();
            assert_eq!(git(&cache, &["rev-parse", "HEAD"]), cold);
            if name == "tag" {
                assert_eq!(cold, git(&a, &["rev-parse", "refs/tags/release"]));
            } else {
                assert_eq!(cold, expected);
            }
            assert!(cache.join("untracked").exists());
        }
        let relative_cache = root.join("relative");
        sync_repo("A", Some("main"), &relative_cache).unwrap();
        sync_repo("A", Some("main"), &relative_cache).unwrap();
        let cache = root.join("branch");
        let before_head = git(&cache, &["rev-parse", "HEAD"]);
        let before_fetch = fs::read(cache.join(".git/FETCH_HEAD")).unwrap();
        let before_config = fs::read(cache.join(".git/config")).unwrap();
        let error = sync_repo(&b_url, Some("main"), &cache)
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not exactly match"));
        assert!(!error.contains(&b_url));
        assert_eq!(git(&cache, &["rev-parse", "HEAD"]), before_head);
        assert_eq!(
            fs::read(cache.join(".git/FETCH_HEAD")).unwrap(),
            before_fetch
        );
        assert_eq!(fs::read(cache.join(".git/config")).unwrap(), before_config);
        assert!(sync_repo(&a_url, Some("missing-ref"), &cache).is_err());
        assert_eq!(git(&cache, &["rev-parse", "HEAD"]), before_head);
        let secret = "https://user:super-secret@example.invalid/B";
        assert!(!sync_repo(secret, None, &cache)
            .unwrap_err()
            .to_string()
            .contains("super-secret"));
        git(&cache, &["config", "remote.origin.url", secret]);
        let error = sync_repo(&a_url, None, &cache).unwrap_err().to_string();
        assert!(!error.contains("super-secret"));
        assert!(error.contains("does not exactly match"));
        git(&cache, &["config", "remote.origin.url", &a_url]);
        let secret_local = format!("file://{}/super-secret-missing", root.display());
        assert!(
            !sync_repo(&secret_local, None, &root.join("missing-source"))
                .unwrap_err()
                .to_string()
                .contains("super-secret")
        );
        assert!(sync_repo(&a_url, Some("--upload-pack=bad"), &root.join("bad-ref")).is_err());
        assert!(sync_repo("--upload-pack=bad", None, &root.join("bad-url")).is_err());
        assert!(!root.join("bad-ref").exists());
        // A normal branch advances on a warm cache, not merely on cold clone.
        fs::write(a.join("content"), "updated").unwrap();
        git(&a, &["commit", "-am", "updated"]);
        sync_repo(&a_url, Some("main"), &cache).unwrap();
        assert_eq!(
            git(&cache, &["rev-parse", "HEAD"]),
            git(&a, &["rev-parse", "HEAD"])
        );
    }
}

#[cfg(test)]
mod version_tests {
    use super::*;

    #[test]
    fn parses_version_management_and_legacy_rollback() {
        for args in [
            vec!["pushit", "version", "list", "hello"],
            vec!["pushit", "version", "activate", "hello", "r123"],
            vec!["pushit", "version", "deactivate", "hello"],
            vec!["pushit", "version", "delete", "hello", "r123", "--yes"],
            vec!["pushit", "rollback", "hello", "--revision", "r123"],
        ] {
            assert!(Cli::try_parse_from(args).is_ok());
        }
        for args in [
            vec!["pushit", "version", "activate", "hello"],
            vec!["pushit", "version", "delete", "hello"],
            vec!["pushit", "version", "deactivate", "hello", "r123"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }
}

#[cfg(test)]
mod owner_login_tests {
    use super::*;
    #[test]
    fn password_destinations_require_https_hub_or_explicit_local_loopback_contract() {
        for server in [
            "http://hub.platform.test",
            "http://192.0.2.1",
            "https://platform.test",
            "https://hub.platform.test/path",
            "https://hub.platform.test?x=1",
            "https://user@hub.platform.test",
            "https://hub.platform.test#x",
            "https://hub.platform.test:0",
            "https://HUB.platform.test",
            "https://hub.platform.test:08443",
            "https://hub.localhost",
            "https://hub.platform.test/../",
            "https://hub.platform.test\\evil",
            "https://hub.-bad.test",
            "https://hub.bad..test",
        ] {
            assert!(builtin_server(server).is_err(), "accepted {server}");
        }
        assert!(builtin_server("https://hub.platform.test").is_ok());
        assert!(builtin_server("https://hub.platform.test:8443").is_ok());
        assert!(local_server("http://hub.localhost:9100").is_ok());
        assert!(local_server("http://hub.platform.test:9100").is_err());
        assert!(Cli::try_parse_from(["pushit", "login", "--builtin", "--local"]).is_err());
    }
}
