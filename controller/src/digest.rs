//! Content digest of deployable artifacts. Computed identically by the
//! controller (from the unpacked revision directory) and by the CLI (from
//! local build outputs), so `pushit plan --artifacts` can prove whether the
//! code on the platform matches a fresh local build — independent of tar
//! metadata or mtimes.
//!
//! The digest covers `app.wasm` plus every file under `dist/` (relative POSIX
//! path + content hash, sorted by path). The controller-generated
//! `dist/config.js` is excluded on both sides: its content is derived from
//! manifest env, not from the build.

use anyhow::Result;
use sha2::{Digest as _, Sha256};
use std::path::{Path, PathBuf};

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn hash_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(hex(&Sha256::digest(&bytes)))
}

/// Every regular file under `dist`, as `(relative POSIX path, absolute path)`,
/// sorted so the digest is stable across filesystems.
fn dist_files(dist: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut files = Vec::new();
    let mut stack = vec![(dist.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            // The controller rewrites dist/config.js at deploy time from
            // manifest env; a local stale copy must not look like code drift.
            if path.is_file() {
                if !(rel == "config.js") {
                    files.push((rel, path));
                }
            } else if path.is_dir() {
                stack.push((path, rel));
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

/// `sha256:<hex>` over the artifact contents (see the module docs).
pub fn artifact_digest(wasm: &Path, dist: Option<&Path>) -> Result<String> {
    let mut acc = String::new();
    acc.push_str("app.wasm ");
    acc.push_str(&hash_file(wasm)?);
    acc.push('\n');
    if let Some(dist) = dist {
        for (rel, path) in dist_files(dist)? {
            acc.push_str("dist/");
            acc.push_str(&rel);
            acc.push(' ');
            acc.push_str(&hash_file(&path)?);
            acc.push('\n');
        }
    }
    Ok(format!("sha256:{}", hex(&Sha256::digest(acc.as_bytes()))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_stable_and_content_addressed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.wasm"), b"wasm-bytes").unwrap();
        let dist = dir.path().join("dist");
        std::fs::create_dir_all(dist.join("assets")).unwrap();
        std::fs::write(dist.join("index.html"), b"<html>").unwrap();
        std::fs::write(dist.join("assets/app.js"), b"console.log(1)").unwrap();
        std::fs::write(dist.join("config.js"), b"stale local config").unwrap();

        let a = artifact_digest(&dir.path().join("app.wasm"), Some(dist.as_path())).unwrap();
        let b = artifact_digest(&dir.path().join("app.wasm"), Some(dist.as_path())).unwrap();
        assert_eq!(a, b);
        assert!(a.starts_with("sha256:"), "{a}");

        // config.js is excluded: rewriting it cannot change the digest
        std::fs::write(dist.join("config.js"), b"regenerated").unwrap();
        assert_eq!(
            a,
            artifact_digest(&dir.path().join("app.wasm"), Some(dist.as_path())).unwrap()
        );
        // any covered content change does change the digest
        std::fs::write(dist.join("index.html"), b"<html>!").unwrap();
        assert_ne!(
            a,
            artifact_digest(&dir.path().join("app.wasm"), Some(dist.as_path())).unwrap()
        );
        // wasm-only digest differs from wasm+dist
        assert_ne!(
            a,
            artifact_digest(&dir.path().join("app.wasm"), None).unwrap()
        );
    }

    #[test]
    fn digest_is_independent_of_directory_entry_order() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        for root in [first.path(), second.path()] {
            std::fs::write(root.join("app.wasm"), b"wasm").unwrap();
            std::fs::create_dir_all(root.join("dist/b")).unwrap();
            std::fs::write(root.join("dist/a.txt"), b"a").unwrap();
            std::fs::write(root.join("dist/b/z.txt"), b"z").unwrap();
            std::fs::write(root.join("dist/b/a.txt"), b"a2").unwrap();
        }
        assert_eq!(
            artifact_digest(
                &first.path().join("app.wasm"),
                Some(&first.path().join("dist"))
            )
            .unwrap(),
            artifact_digest(
                &second.path().join("app.wasm"),
                Some(&second.path().join("dist"))
            )
            .unwrap()
        );
    }
}
