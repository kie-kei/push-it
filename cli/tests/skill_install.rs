use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Sandbox(PathBuf);
impl Sandbox {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "pushit-skill-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(path.join("home")).unwrap();
        fs::create_dir_all(path.join("project")).unwrap();
        // Exercise a distributed binary, not `cargo run` or the repository cwd.
        fs::copy(env!("CARGO_BIN_EXE_pushit"), path.join("pushit")).unwrap();
        Self(path)
    }
    fn run(&self, args: &[&str], answer: &str) -> String {
        let mut child = Command::new(self.0.join("pushit"))
            .args(args)
            .current_dir(self.0.join("project"))
            .env_clear()
            .env("HOME", self.0.join("home"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(answer.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{:?}", output);
        String::from_utf8(output.stdout).unwrap()
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn assert_bundle(dir: &Path) {
    let skill = include_str!("../../skills/pushit-app/SKILL.md");
    assert_eq!(fs::read_to_string(dir.join("SKILL.md")).unwrap(), skill);
    for (name, expected) in [
        (
            "sql.md",
            include_str!("../../skills/pushit-app/references/sql.md"),
        ),
        (
            "blobstore.md",
            include_str!("../../skills/pushit-app/references/blobstore.md"),
        ),
        (
            "logging.md",
            include_str!("../../skills/pushit-app/references/logging.md"),
        ),
    ] {
        assert!(skill.contains(&format!("(references/{name})")));
        assert_eq!(
            fs::read_to_string(dir.join("references").join(name)).unwrap(),
            expected
        );
    }
}

#[test]
fn installs_complete_user_bundle_without_touching_project() {
    let s = Sandbox::new();
    let output = s.run(&["skill", "install"], "yes\n");
    let dest = s.0.join("home/.agents/skills/pushit");
    assert_bundle(&dest);
    assert!(output.contains(&dest.display().to_string()));
    assert!(!s.0.join("project/.agents").exists());
    assert!(!s.0.join("home/.agents/skill").exists());
}

#[test]
fn project_install_confirms_overwrite_and_preserves_unmanaged_files() {
    let s = Sandbox::new();
    let dest = s.0.join("project/.agents/skills/pushit");
    let legacy = s.0.join("project/.agents/skill/pushit");
    fs::create_dir_all(&legacy).unwrap();
    fs::write(legacy.join("SKILL.md"), "legacy custom skill").unwrap();
    s.run(&["skill", "install", "--project"], "y\n");
    assert_bundle(&dest);
    fs::write(dest.join("SKILL.md"), "custom skill").unwrap();
    fs::write(dest.join("references/sql.md"), "custom reference").unwrap();
    fs::write(dest.join("notes.txt"), "keep me").unwrap();
    let declined = s.run(&["skill", "install", "--project"], "n\n");
    assert!(declined.contains("will overwrite") && declined.contains("aborted"));
    assert_eq!(
        fs::read_to_string(dest.join("SKILL.md")).unwrap(),
        "custom skill"
    );
    assert_eq!(
        fs::read_to_string(dest.join("references/sql.md")).unwrap(),
        "custom reference"
    );
    s.run(&["skill", "install", "--project"], "yes\n");
    assert_bundle(&dest);
    assert_eq!(
        fs::read_to_string(dest.join("notes.txt")).unwrap(),
        "keep me"
    );
    assert_eq!(
        fs::read_to_string(legacy.join("SKILL.md")).unwrap(),
        "legacy custom skill"
    );
    assert!(!s.0.join("home/.agents").exists());
}

#[test]
fn decline_creates_nothing_and_help_names_discoverable_paths() {
    let s = Sandbox::new();
    for args in [
        &["skill", "install"][..],
        &["skill", "install", "--project"][..],
    ] {
        assert!(s.run(args, "\n").contains("aborted"));
    }
    assert!(!s.0.join("home/.agents").exists());
    assert!(!s.0.join("project/.agents").exists());
    let help = s.run(&["skill", "install", "--help"], "");
    assert!(help.contains(".agents/skills/pushit"));
    assert!(!help.contains(".agents/skill/"));
}
