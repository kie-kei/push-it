use anyhow::{bail, Result};
use reqwest::blocking::Client;
use serde::Serialize;
use std::{process::Command, time::Duration};

#[derive(Serialize)]
struct Check {
    name: String,
    status: &'static str,
    detail: String,
}
#[derive(Serialize)]
struct Report {
    checks: Vec<Check>,
    ok: bool,
}
impl Report {
    fn new(checks: Vec<Check>) -> Self {
        let ok = checks.iter().all(|c| c.status != "fail");
        Self { checks, ok }
    }
}
fn matches_version(text: &str, expected: &str) -> bool {
    text.split_whitespace().any(|word| word == expected)
}
fn version(name: &str, args: &[&str], expected: Option<&str>, missing: &'static str) -> Check {
    let output = Command::new(name).args(args).output();
    let (status, detail) = match output {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let matches = expected.is_none_or(|v| matches_version(&text, v));
            (if matches { "ok" } else { "warn" }, text)
        }
        _ => (
            missing,
            format!(
                "{} unavailable",
                std::iter::once(name)
                    .chain(args.iter().copied())
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
        ),
    };
    Check {
        name: name.into(),
        status,
        detail,
    }
}
pub fn run(server: &str, client: &Client, json: bool) -> Result<()> {
    let mut checks = vec![
        version("cargo", &["--version"], None, "fail"),
        version("rustc", &["--version"], Some("1.99.1"), "fail"),
        version("cargo", &["component", "--version"], Some("0.21.1"), "fail"),
    ];
    checks[2].name = "cargo-component".into();
    let target = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output();
    let installed = target.is_ok_and(|o| {
        o.status.success()
            && String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l == "wasm32-wasip1")
    });
    checks.push(Check {
        name: "wasm32-wasip1".into(),
        status: if installed { "ok" } else { "warn" },
        detail: if installed {
            "target installed"
        } else {
            "install with `rustup target add wasm32-wasip1`"
        }
        .into(),
    });
    checks.push(version("git", &["--version"], None, "warn"));
    let probe = Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let (status, detail) = match probe.get(format!("{server}/auth/mode")).send() {
        Err(_) => ("fail", "cannot reach controller".into()),
        Ok(response) => {
            let success = response.status().is_success();
            let mode = response
                .json::<serde_json::Value>()
                .ok()
                .and_then(|j| j["mode"].as_str().map(str::to_owned));
            match mode
                .filter(|_| success)
                .filter(|m| m == "local" || m == "builtin")
            {
                Some(mode) => ("ok", format!("controller authentication mode: {mode}")),
                None => (
                    "warn",
                    "not a standalone owner controller (OIDC/dev login applies)".into(),
                ),
            }
        }
    };
    checks.push(Check {
        name: "server".into(),
        status,
        detail,
    });
    let (status, detail) = match client
        .get(format!("{server}/api/apps"))
        .timeout(Duration::from_secs(5))
        .send()
    {
        Ok(r) if r.status() == 200 => ("ok", "authenticated API reachable".into()),
        Ok(r) if r.status() == 401 => (
            "fail",
            "run `pushit login` (or use --dev with a PUSHIT_DEV=1 controller)".into(),
        ),
        Ok(r) => ("fail", format!("API returned HTTP {}", r.status())),
        Err(_) => ("fail", "cannot reach authenticated API".into()),
    };
    checks.push(Check {
        name: "api".into(),
        status,
        detail,
    });
    let (status, detail) = match std::fs::read_to_string("pushit.yaml") {
        Ok(text) => match pushit_controller::manifest::Manifest::parse(&text) {
            Ok(m) => ("ok", format!("manifest name: {}", m.name)),
            Err(e) => ("warn", format!("manifest: {e:#}")),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            ("ok", "skipped: no local pushit.yaml".into())
        }
        Err(e) => ("warn", format!("read manifest: {e}")),
    };
    checks.push(Check {
        name: "manifest".into(),
        status,
        detail,
    });
    let report = Report::new(checks);
    if json {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        for c in &report.checks {
            println!("{} {}: {}", c.status, c.name, c.detail);
        }
    }
    if !report.ok {
        bail!("doctor found failing checks");
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn warnings_do_not_fail_and_json_has_stable_shape() {
        let report = Report::new(vec![Check {
            name: "test".into(),
            status: "warn",
            detail: "other version".into(),
        }]);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["checks"][0]["status"], "warn");
        assert!(
            !Report::new(vec![Check {
                name: "test".into(),
                status: "fail",
                detail: "missing".into()
            }])
            .ok
        );
    }
    #[test]
    fn versions_match_whole_tokens_not_prefixes() {
        assert!(matches_version("rustc 1.99.1 (hash)", "1.99.1"));
        assert!(matches_version("cargo-component 0.21.1", "0.21.1"));
        assert!(!matches_version("rustc 1.99.10", "1.99.1"));
        assert!(!matches_version("rustc 1.99.1-nightly", "1.99.1"));
    }
}
