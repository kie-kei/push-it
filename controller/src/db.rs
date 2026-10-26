use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Weak};

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
    // The registry mutex serializes snapshot+pin with deletion+busy-check.
    // Never held across await; weak entries do not retain deleted versions.
    artifacts: Arc<Mutex<HashMap<String, Weak<tokio::sync::RwLock<()>>>>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DeleteVersion {
    Missing,
    Active,
    Busy,
    Deleted(String),
}

#[derive(Debug, Serialize, Clone)]
pub struct App {
    pub name: String,
    pub domain: String,
    pub manifest: String,
    pub created_at: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct Revision {
    pub id: String,
    pub app: String,
    pub artifact_dir: String,
    pub created_at: String,
    pub active: bool,
    /// None for historical revisions whose configuration cannot be recovered.
    pub manifest: Option<String>,
    pub domain: Option<String>,
    /// Content digest of the deployed artifacts; None for legacy rows.
    pub digest: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AppRow {
    pub name: String,
    pub domain: String,
    pub revision: String,
    pub deployed_at: String,
    pub status: String,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS apps(
               name TEXT PRIMARY KEY,
               domain TEXT NOT NULL,
               manifest TEXT NOT NULL,
               created_at TEXT NOT NULL DEFAULT (datetime('now')));
             CREATE TABLE IF NOT EXISTS revisions(
               id TEXT NOT NULL,
               app TEXT NOT NULL,
               artifact_dir TEXT NOT NULL,
               created_at TEXT NOT NULL DEFAULT (datetime('now')),
               active INTEGER NOT NULL DEFAULT 0,
               PRIMARY KEY(app, id));
             CREATE TABLE IF NOT EXISTS secrets(
               app TEXT NOT NULL,
               key TEXT NOT NULL,
               value_enc BLOB NOT NULL,
               PRIMARY KEY(app, key));
             CREATE TABLE IF NOT EXISTS global_secrets(
               key TEXT PRIMARY KEY,
               value_enc BLOB NOT NULL);",
        )?;
        // Migrate under a write lock. Capture the currently running legacy
        // configuration; historical deploy manifests cannot be reconstructed.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let has_manifest = {
            let mut st = tx.prepare("PRAGMA table_info(revisions)")?;
            let names = st
                .query_map([], |r| r.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            names.iter().any(|name| name == "manifest")
        };
        if !has_manifest {
            tx.execute_batch(
                "ALTER TABLE revisions ADD COLUMN manifest TEXT;
                 ALTER TABLE revisions ADD COLUMN domain TEXT;
                 UPDATE revisions SET
                   manifest=(SELECT manifest FROM apps WHERE name=revisions.app),
                   domain=(SELECT domain FROM apps WHERE name=revisions.app)
                 WHERE active=1;",
            )?;
        }
        let has_digest = {
            let mut st = tx.prepare("PRAGMA table_info(revisions)")?;
            let names = st
                .query_map([], |r| r.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            names.iter().any(|name| name == "digest")
        };
        if !has_digest {
            // Digests of already-deployed artifacts are recomputable from the
            // unpacked revision directories; fill them in opportunistically.
            tx.execute_batch("ALTER TABLE revisions ADD COLUMN digest TEXT;")?;
        }
        tx.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS one_active_revision_per_app
             ON revisions(app) WHERE active=1;
             CREATE TRIGGER IF NOT EXISTS immutable_revision_snapshot
             BEFORE UPDATE OF id, app, artifact_dir, manifest, domain ON revisions
             BEGIN SELECT RAISE(ABORT, 'revision snapshots are immutable'); END;",
        )
        .context(
            "revision migration failed: repair duplicate active revisions before restarting",
        )?;
        tx.commit()?;
        Ok(Db {
            conn: Arc::new(Mutex::new(conn)),
            artifacts: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Best-effort digest backfill for legacy revisions: computes
    /// `artifact_digest` from each stored artifact directory that still has
    /// its app.wasm. Missing artifacts keep digest NULL (unknown).
    pub fn backfill_digests(&self) {
        let rows: Vec<(String, String, String)> = match self
            .c()
            .prepare("SELECT id, app, artifact_dir FROM revisions WHERE digest IS NULL")
        {
            Ok(mut st) => st
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        for (id, app, dir) in rows {
            let wasm = std::path::Path::new(&dir).join("app.wasm");
            if !wasm.is_file() {
                continue;
            }
            let dist = std::path::Path::new(&dir).join("dist");
            let dist = dist.is_dir().then_some(dist);
            if let Ok(digest) = crate::digest::artifact_digest(&wasm, dist.as_deref()) {
                let _ = self.c().execute(
                    "UPDATE revisions SET digest=?1 WHERE app=?2 AND id=?3",
                    params![digest, app, id],
                );
            }
        }
    }

    fn artifact_lock(
        locks: &mut HashMap<String, Weak<tokio::sync::RwLock<()>>>,
        dir: &str,
    ) -> Arc<tokio::sync::RwLock<()>> {
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(dir).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::RwLock::new(()));
        locks.insert(dir.to_owned(), Arc::downgrade(&lock));
        lock
    }

    pub fn pin_active_by_domain(
        &self,
        domain: &str,
    ) -> Result<Option<(App, Revision, tokio::sync::OwnedRwLockReadGuard<()>)>> {
        let mut locks = self.artifacts.lock();
        let Some((app, revision)) = self.active_app_by_domain(domain)? else {
            return Ok(None);
        };
        let pin = Self::artifact_lock(&mut locks, &revision.artifact_dir)
            .try_read_owned()
            .context("version artifacts are being removed; retry")?;
        Ok(Some((app, revision, pin)))
    }

    pub fn pin_revision(
        &self,
        app: &str,
        id: &str,
    ) -> Result<Option<(Revision, tokio::sync::OwnedRwLockReadGuard<()>)>> {
        let mut locks = self.artifacts.lock();
        let Some(revision) = self.revision(app, id)? else {
            return Ok(None);
        };
        let pin = Self::artifact_lock(&mut locks, &revision.artifact_dir)
            .try_read_owned()
            .context("version artifacts are being removed; retry")?;
        Ok(Some((revision, pin)))
    }

    fn c(&self) -> parking_lot::MutexGuard<'_, Connection> {
        self.conn.lock()
    }

    // ---- apps ----

    pub fn get_app(&self, name: &str) -> Result<Option<App>> {
        let app = self
            .c()
            .query_row(
                "SELECT name,domain,manifest,created_at FROM apps WHERE name=?1",
                params![name],
                |r| {
                    Ok(App {
                        name: r.get(0)?,
                        domain: r.get(1)?,
                        manifest: r.get(2)?,
                        created_at: r.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(app)
    }

    pub fn list_apps(&self) -> Result<Vec<AppRow>> {
        let conn = self.c();
        let mut st = conn.prepare(
            "SELECT a.name, a.domain,
                    COALESCE((SELECT r.id FROM revisions r WHERE r.app=a.name AND r.active=1),''),
                    COALESCE((SELECT r.created_at FROM revisions r WHERE r.app=a.name AND r.active=1), a.created_at)
             FROM apps a ORDER BY a.name",
        )?;
        let rows = st
            .query_map([], |r| {
                Ok(AppRow {
                    name: r.get(0)?,
                    domain: r.get(1)?,
                    revision: r.get(2)?,
                    deployed_at: r.get(3)?,
                    status: if r.get::<_, String>(2)?.is_empty() {
                        "inactive"
                    } else {
                        "ok"
                    }
                    .into(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn delete_app(&self, name: &str) -> Result<Vec<String>> {
        let mut locks = self.artifacts.lock();
        let mut conn = self.c();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let dirs = {
            let mut st = tx.prepare("SELECT artifact_dir FROM revisions WHERE app=?1")?;
            let dirs = st
                .query_map(params![name], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            dirs
        };
        let _pins = dirs
            .iter()
            .map(|dir| Self::artifact_lock(&mut locks, dir).try_write_owned())
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("app has in-flight requests; retry deletion after they finish")?;
        tx.execute("DELETE FROM revisions WHERE app=?1", params![name])?;
        tx.execute("DELETE FROM secrets WHERE app=?1", params![name])?;
        tx.execute("DELETE FROM apps WHERE name=?1", params![name])?;
        tx.commit()?;
        Ok(dirs)
    }

    // ---- revisions ----

    /// Publish the artifact and its complete configuration in one transaction.
    /// Callers must finish and validate the immutable artifact directory first.
    /// Legacy convenience: no artifact digest recorded (see `backfill_digests`).
    pub fn deploy_revision(
        &self,
        app: &str,
        id: &str,
        dir: &str,
        domain: &str,
        manifest: &str,
    ) -> Result<()> {
        self.deploy_revision_with_digest(app, id, dir, domain, manifest, None)
    }

    /// Same, but records the artifact content digest (`digest.rs`) for later
    /// `plan --artifacts` comparisons; pass the digest computed from the
    /// unpacked revision directory before calling this.
    pub fn deploy_revision_with_digest(
        &self,
        app: &str,
        id: &str,
        dir: &str,
        domain: &str,
        manifest: &str,
        digest: Option<&str>,
    ) -> Result<()> {
        let mut conn = self.c();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO apps(name,domain,manifest) VALUES(?1,?2,?3)
             ON CONFLICT(name) DO UPDATE SET domain=?2, manifest=?3",
            params![app, domain, manifest],
        )?;
        tx.execute(
            "INSERT INTO revisions(id,app,artifact_dir,manifest,domain,digest,active)
             VALUES(?1,?2,?3,?4,?5,?6,0)",
            params![id, app, dir, manifest, domain, digest],
        )?;
        Self::activate_in(&tx, app, id)?;
        tx.commit()?;
        Ok(())
    }

    pub fn activate(&self, app: &str, id: &str) -> Result<()> {
        let mut conn = self.c();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::activate_in(&tx, app, id)?;
        tx.commit()?;
        Ok(())
    }

    /// Zero active versions is a valid app state; retain its configuration,
    /// secrets, data and all immutable versions for later activation.
    pub fn deactivate(&self, app: &str) -> Result<bool> {
        let mut conn = self.c();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM apps WHERE name=?1)",
            params![app],
            |r| r.get(0),
        )?;
        if !exists {
            return Ok(false);
        }
        tx.execute(
            "UPDATE revisions SET active=0 WHERE app=?1 AND active=1",
            params![app],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Check and remove in the same write transaction, so activation/deploy can
    /// never race the active-version guard. No snapshot is required for deletion.
    pub fn delete_version(&self, app: &str, id: &str) -> Result<DeleteVersion> {
        let mut locks = self.artifacts.lock();
        let mut conn = self.c();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let found: Option<(String, bool)> = tx
            .query_row(
                "SELECT artifact_dir,active FROM revisions WHERE app=?1 AND id=?2",
                params![app, id],
                |r| Ok((r.get(0)?, r.get::<_, i32>(1)? != 0)),
            )
            .optional()?;
        let Some((dir, active)) = found else {
            return Ok(DeleteVersion::Missing);
        };
        if active {
            return Ok(DeleteVersion::Active);
        }
        let Ok(_pin) = Self::artifact_lock(&mut locks, &dir).try_write_owned() else {
            return Ok(DeleteVersion::Busy);
        };
        tx.execute(
            "DELETE FROM revisions WHERE app=?1 AND id=?2 AND active=0",
            params![app, id],
        )?;
        tx.commit()?;
        Ok(DeleteVersion::Deleted(dir))
    }

    fn activate_in(conn: &Connection, app: &str, id: &str) -> Result<()> {
        let snapshot: Option<(Option<String>, Option<String>)> = conn
            .query_row(
                "SELECT manifest,domain FROM revisions WHERE app=?1 AND id=?2",
                params![app, id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((Some(manifest), Some(domain))) = snapshot else {
            bail!("revision '{id}' for app '{app}' is missing or has no historical manifest; redeploy the desired version");
        };
        let conflict: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM apps a JOIN revisions r ON r.app=a.name AND r.active=1
             WHERE a.name!=?1 AND a.domain=?2 COLLATE NOCASE)",
            params![app, domain],
            |r| r.get(0),
        )?;
        if conflict {
            bail!("domain '{domain}' is already active for another app");
        }
        conn.execute(
            "UPDATE revisions SET active=0 WHERE app=?1 AND active=1",
            params![app],
        )?;
        conn.execute(
            "UPDATE revisions SET active=1 WHERE app=?1 AND id=?2",
            params![app, id],
        )?;
        let updated = conn.execute(
            "UPDATE apps SET manifest=?2,domain=?3 WHERE name=?1",
            params![app, manifest, domain],
        )?;
        if updated != 1 {
            bail!("missing app '{app}'");
        }
        Ok(())
    }

    fn active_snapshot(r: &rusqlite::Row<'_>) -> rusqlite::Result<(App, Revision)> {
        Ok((
            App {
                name: r.get(0)?,
                domain: r.get(1)?,
                manifest: r.get(2)?,
                created_at: r.get(3)?,
            },
            Revision {
                id: r.get(4)?,
                app: r.get(5)?,
                artifact_dir: r.get(6)?,
                created_at: r.get(7)?,
                active: r.get::<_, i32>(8)? != 0,
                manifest: r.get(9)?,
                domain: r.get(10)?,
                digest: r.get(11)?,
            },
        ))
    }

    pub fn active_apps(&self) -> Result<Vec<(App, Revision)>> {
        let conn = self.c();
        let mut statement = conn.prepare(
            "SELECT a.name,a.domain,a.manifest,a.created_at,
                    r.id,r.app,r.artifact_dir,r.created_at,r.active,r.manifest,r.domain,r.digest
             FROM apps a JOIN revisions r ON r.app=a.name AND r.active=1
             ORDER BY a.name",
        )?;
        let rows = statement.query_map([], Self::active_snapshot)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Resolve routing, artifact and configuration from a single database snapshot.
    pub fn active_app_by_domain(&self, domain: &str) -> Result<Option<(App, Revision)>> {
        Ok(self
            .c()
            .query_row(
                "SELECT a.name,a.domain,a.manifest,a.created_at,
                    r.id,r.app,r.artifact_dir,r.created_at,r.active,r.manifest,r.domain,r.digest
             FROM apps a JOIN revisions r ON r.app=a.name AND r.active=1
             WHERE a.domain=?1 COLLATE NOCASE LIMIT 1",
                params![domain],
                Self::active_snapshot,
            )
            .optional()?)
    }

    /// Previous revision = newest non-active revision with a known snapshot.
    pub fn previous_revision(&self, app: &str) -> Result<Option<String>> {
        self.c()
            .query_row(
                "SELECT id FROM revisions WHERE app=?1 AND active=0 AND manifest IS NOT NULL AND domain IS NOT NULL
                 ORDER BY created_at DESC, id DESC LIMIT 1",
                params![app],
                |r| r.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn revisions(&self, app: &str) -> Result<Vec<Revision>> {
        let conn = self.c();
        let mut st = conn.prepare(
            "SELECT id,app,artifact_dir,created_at,active,manifest,domain,digest FROM revisions
             WHERE app=?1 ORDER BY created_at DESC, id DESC",
        )?;
        let rows = st
            .query_map(params![app], |r| {
                Ok(Revision {
                    id: r.get(0)?,
                    app: r.get(1)?,
                    artifact_dir: r.get(2)?,
                    created_at: r.get(3)?,
                    active: r.get::<_, i32>(4)? != 0,
                    manifest: r.get(5)?,
                    domain: r.get(6)?,
                    digest: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn active_revision(&self, app: &str) -> Result<Option<Revision>> {
        Ok(self
            .c()
            .query_row(
                "SELECT id,app,artifact_dir,created_at,active,manifest,domain,digest FROM revisions
                 WHERE app=?1 AND active=1",
                params![app],
                |r| {
                    Ok(Revision {
                        id: r.get(0)?,
                        app: r.get(1)?,
                        artifact_dir: r.get(2)?,
                        created_at: r.get(3)?,
                        active: true,
                        manifest: r.get(5)?,
                        domain: r.get(6)?,
                        digest: r.get(7)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn revision(&self, app: &str, id: &str) -> Result<Option<Revision>> {
        Ok(self.c().query_row(
            "SELECT id,app,artifact_dir,created_at,active,manifest,domain,digest FROM revisions WHERE app=?1 AND id=?2",
            params![app,id], |r| Ok(Revision {
                id:r.get(0)?,app:r.get(1)?,artifact_dir:r.get(2)?,created_at:r.get(3)?,
                active:r.get::<_,i32>(4)? != 0,manifest:r.get(5)?,domain:r.get(6)?,
                digest:r.get(7)?,
            }),
        ).optional()?)
    }

    // ---- secrets ----

    pub fn set_secret(&self, app: &str, key: &str, enc: &[u8]) -> Result<()> {
        self.c().execute(
            "INSERT INTO secrets(app,key,value_enc) VALUES(?1,?2,?3)
             ON CONFLICT(app,key) DO UPDATE SET value_enc=?3",
            params![app, key, enc],
        )?;
        Ok(())
    }

    pub fn delete_secret(&self, app: &str, key: &str) -> Result<()> {
        self.c().execute(
            "DELETE FROM secrets WHERE app=?1 AND key=?2",
            params![app, key],
        )?;
        Ok(())
    }

    pub fn secret_names(&self, app: &str) -> Result<Vec<String>> {
        let conn = self.c();
        let mut st = conn.prepare("SELECT key FROM secrets WHERE app=?1 ORDER BY key")?;
        let rows = st
            .query_map(params![app], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(rows)
    }

    pub fn get_secret_enc(&self, app: &str, key: &str) -> Result<Option<Vec<u8>>> {
        self.c()
            .query_row(
                "SELECT value_enc FROM secrets WHERE app=?1 AND key=?2",
                params![app, key],
                |r| r.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn all_secrets(&self, app: &str) -> Result<Vec<(String, Vec<u8>)>> {
        let conn = self.c();
        let mut st = conn.prepare("SELECT key, value_enc FROM secrets WHERE app=?1")?;
        let rows = st
            .query_map(params![app], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<(String, Vec<u8>)>>>()?;
        Ok(rows)
    }

    // ---- platform-wide (global) secrets ----

    pub fn set_global_secret(&self, key: &str, enc: &[u8]) -> Result<()> {
        self.c().execute(
            "INSERT INTO global_secrets(key,value_enc) VALUES(?1,?2)
             ON CONFLICT(key) DO UPDATE SET value_enc=?2",
            params![key, enc],
        )?;
        Ok(())
    }

    pub fn delete_global_secret(&self, key: &str) -> Result<()> {
        self.c()
            .execute("DELETE FROM global_secrets WHERE key=?1", params![key])?;
        Ok(())
    }

    pub fn global_secret_names(&self) -> Result<Vec<String>> {
        let conn = self.c();
        let mut st = conn.prepare("SELECT key FROM global_secrets ORDER BY key")?;
        let rows = st
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(rows)
    }

    pub fn get_global_secret(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.c()
            .query_row(
                "SELECT value_enc FROM global_secrets WHERE key=?1",
                params![key],
                |r| r.get(0),
            )
            .optional()
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory() -> Db {
        Db::open(Path::new(":memory:")).unwrap()
    }

    fn deploy(db: &Db, id: &str) {
        db.deploy_revision(
            "a",
            id,
            &format!("/x/{id}"),
            &format!("{id}.localhost"),
            &format!("name: a\nenv:\n  VERSION: {id}"),
        )
        .unwrap();
    }

    fn assert_active(db: &Db, id: &str) {
        let app = db.get_app("a").unwrap().unwrap();
        let rev = db.active_revision("a").unwrap().unwrap();
        assert_eq!(rev.id, id);
        assert_eq!(app.domain, format!("{id}.localhost"));
        assert_eq!(Some(app.manifest), rev.manifest);
        assert_eq!(Some(app.domain), rev.domain);
        assert_eq!(
            db.revisions("a")
                .unwrap()
                .iter()
                .filter(|r| r.active)
                .count(),
            1
        );
    }

    #[test]
    fn cron_active_apps_returns_only_active_configuration_snapshots() {
        let db = memory();
        assert!(db.active_apps().unwrap().is_empty());
        deploy(&db, "r1");
        deploy(&db, "r2");
        db.deploy_revision("b", "b1", "/x/b1", "b.localhost", "name: b")
            .unwrap();
        let apps = db.active_apps().unwrap();
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].0.name, "a");
        assert_eq!(apps[0].1.id, "r2");
        assert_eq!(Some(apps[0].0.manifest.clone()), apps[0].1.manifest);
        db.deactivate("a").unwrap();
        assert_eq!(db.active_apps().unwrap()[0].0.name, "b");
        db.activate("a", "r1").unwrap();
        assert_eq!(db.active_apps().unwrap()[0].1.id, "r1");
        db.delete_app("a").unwrap();
        assert_eq!(db.active_apps().unwrap().len(), 1);
    }

    #[test]
    fn version_lifecycle_keeps_data_secrets_and_other_versions() {
        let db = memory();
        deploy(&db, "r1");
        deploy(&db, "r2");
        db.set_secret("a", "TOKEN", b"value").unwrap();
        assert_eq!(db.delete_version("a", "r2").unwrap(), DeleteVersion::Active);
        assert!(db.deactivate("a").unwrap());
        assert!(db.deactivate("a").unwrap()); // idempotent
        assert!(!db.deactivate("missing").unwrap());
        assert!(db.active_revision("a").unwrap().is_none());
        assert!(db.active_app_by_domain("r2.localhost").unwrap().is_none());
        assert_eq!(db.list_apps().unwrap()[0].status, "inactive");
        db.activate("a", "r1").unwrap();
        assert_active(&db, "r1");
        assert_eq!(
            db.delete_version("a", "r2").unwrap(),
            DeleteVersion::Deleted("/x/r2".into())
        );
        assert_eq!(
            db.delete_version("a", "r2").unwrap(),
            DeleteVersion::Missing
        );
        assert_eq!(db.revisions("a").unwrap().len(), 1);
        assert_eq!(db.get_secret_enc("a", "TOKEN").unwrap().unwrap(), b"value");
    }

    #[test]
    fn deletion_pins_are_per_version_and_never_block_unrelated_traffic() {
        let db = memory();
        deploy(&db, "r1");
        let (_, _, pin) = db.pin_active_by_domain("r1.localhost").unwrap().unwrap();
        deploy(&db, "r2");
        assert_eq!(db.delete_version("a", "r1").unwrap(), DeleteVersion::Busy);
        let (_, _, other_pin) = db.pin_active_by_domain("r2.localhost").unwrap().unwrap();
        assert!(db.delete_app("a").is_err());
        drop(pin);
        assert_eq!(
            db.delete_version("a", "r1").unwrap(),
            DeleteVersion::Deleted("/x/r1".into())
        );
        assert!(db.pin_revision("a", "r1").unwrap().is_none());
        assert_eq!(db.active_revision("a").unwrap().unwrap().id, "r2");
        drop(other_pin);
    }

    #[test]
    fn legacy_inactive_version_without_manifest_can_be_deleted() {
        let db = memory();
        deploy(&db, "r1");
        db.c().execute("INSERT INTO revisions(id,app,artifact_dir,active) VALUES('legacy','a','/x/legacy',0)", []).unwrap();
        assert!(db.activate("a", "legacy").is_err());
        assert_eq!(
            db.delete_version("a", "legacy").unwrap(),
            DeleteVersion::Deleted("/x/legacy".into())
        );
        assert_active(&db, "r1");
    }

    #[test]
    fn versions_sort_deterministically_and_domains_cannot_steal_active_routes() {
        let db = memory();
        deploy(&db, "r1");
        deploy(&db, "r2");
        assert_eq!(
            db.revisions("a")
                .unwrap()
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            vec!["r2", "r1"]
        );
        assert!(db
            .deploy_revision("b", "b1", "/b/1", "R2.LOCALHOST", "name: b")
            .is_err());
        assert!(db.get_app("b").unwrap().is_none());
        assert_active(&db, "r2");
        db.deactivate("a").unwrap();
        db.deploy_revision("b", "b1", "/b/1", "r2.localhost", "name: b")
            .unwrap();
        assert!(db.activate("a", "r2").is_err());
        assert!(db.active_revision("a").unwrap().is_none());
    }

    #[test]
    fn concurrent_activation_deletion_and_deploy_preserve_invariants() {
        let db = memory();
        deploy(&db, "r1");
        deploy(&db, "r2");
        let barrier = Arc::new(std::sync::Barrier::new(4));
        let mut threads = Vec::new();
        for action in 0..3 {
            let db = db.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                for i in 0..30 {
                    match action {
                        0 => {
                            let _ = db.activate("a", "r1");
                        }
                        1 => {
                            let _ = db.delete_version("a", "r1");
                        }
                        _ => {
                            deploy(&db, &format!("new{i:02}"));
                        }
                    }
                }
            }));
        }
        barrier.wait();
        for thread in threads {
            thread.join().unwrap();
        }
        let revisions = db.revisions("a").unwrap();
        let active = revisions.iter().filter(|r| r.active).collect::<Vec<_>>();
        assert_eq!(active.len(), 1);
        let app = db.get_app("a").unwrap().unwrap();
        assert_eq!(active[0].domain.as_deref(), Some(app.domain.as_str()));
        assert_eq!(active[0].manifest.as_deref(), Some(app.manifest.as_str()));
    }

    #[test]
    fn deletion_returns_only_removed_snapshot_directories() {
        let db = Db::open(Path::new(":memory:")).unwrap();
        db.deploy_revision("a", "old", "/apps/a/old", "a.localhost", "name: a")
            .unwrap();
        let cleanup = db.delete_app("a").unwrap();
        db.deploy_revision("a", "new", "/apps/a/new", "a.localhost", "name: a")
            .unwrap();
        assert_eq!(cleanup, vec!["/apps/a/old"]);
        assert_eq!(db.active_revision("a").unwrap().unwrap().id, "new");
    }

    #[test]
    fn rollback_restores_manifest_domain_artifacts_but_not_secret_values() {
        let db = memory();
        deploy(&db, "r1");
        db.set_secret("a", "TOKEN", b"old").unwrap();
        deploy(&db, "r2");
        db.set_secret("a", "TOKEN", b"new").unwrap();
        assert_active(&db, "r2");
        assert_eq!(db.previous_revision("a").unwrap().as_deref(), Some("r1"));
        db.activate("a", "r1").unwrap();
        assert_active(&db, "r1");
        let (app, rev) = db.active_app_by_domain("r1.localhost").unwrap().unwrap();
        assert_eq!(app.manifest, "name: a\nenv:\n  VERSION: r1");
        assert_eq!(rev.artifact_dir, "/x/r1");
        assert!(db.active_app_by_domain("r2.localhost").unwrap().is_none());
        assert_eq!(db.get_secret_enc("a", "TOKEN").unwrap().unwrap(), b"new");
        assert!(db.activate("a", "missing").is_err());
        assert_active(&db, "r1");
    }

    #[test]
    fn failed_activation_and_deployment_restore_previous_state() {
        let db = memory();
        deploy(&db, "r1");
        deploy(&db, "r2");
        db.c()
            .execute_batch(
                "CREATE TRIGGER fail_activation BEFORE UPDATE OF active ON revisions
            WHEN NEW.active=1 AND NEW.id!='r2'
            BEGIN SELECT RAISE(ABORT, 'injected activation failure'); END;",
            )
            .unwrap();
        assert!(db.activate("a", "r1").is_err());
        assert_active(&db, "r2");
        assert!(db
            .deploy_revision("a", "r3", "/x/r3", "r3.localhost", "name: a")
            .is_err());
        assert_active(&db, "r2");
        assert_eq!(db.revisions("a").unwrap().len(), 2);
        assert!(db
            .deploy_revision("b", "r3", "/b/r3", "b.localhost", "name: b")
            .is_err());
        assert!(db.get_app("b").unwrap().is_none());
        assert!(db.revisions("b").unwrap().is_empty());
        assert!(db
            .deploy_revision("a", "r2", "/replacement", "bad", "bad")
            .is_err());
        assert_active(&db, "r2");
    }

    #[test]
    fn snapshots_are_immutable_and_unique_index_rejects_two_active_revisions() {
        let db = memory();
        deploy(&db, "r1");
        deploy(&db, "r2");
        assert!(db
            .c()
            .execute("UPDATE revisions SET manifest='changed' WHERE id='r1'", [])
            .is_err());
        assert!(db
            .c()
            .execute(
                "UPDATE revisions SET artifact_dir='/changed' WHERE id='r1'",
                []
            )
            .is_err());
        assert!(db
            .c()
            .execute("UPDATE revisions SET active=1 WHERE id='r1'", [])
            .is_err());
        assert_active(&db, "r2");
    }

    #[test]
    fn concurrent_switches_on_independent_connections_preserve_one_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite");
        let db = Db::open(&path).unwrap();
        db.deploy_revision("a", "r1", "/x/r1", "a.localhost", "manifest r1")
            .unwrap();
        db.deploy_revision("a", "r2", "/x/r2", "a.localhost", "manifest r2")
            .unwrap();
        let other = Db::open(&path).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let b = barrier.clone();
        let worker = std::thread::spawn(move || {
            b.wait();
            for _ in 0..30 {
                other.activate("a", "r1").unwrap();
            }
        });
        barrier.wait();
        for _ in 0..30 {
            db.activate("a", "r2").unwrap();
            let (app, rev) = db.active_app_by_domain("a.localhost").unwrap().unwrap();
            assert_eq!(Some(app.manifest), rev.manifest);
            assert_eq!(Some(app.domain), rev.domain);
        }
        worker.join().unwrap();
        assert_eq!(
            db.revisions("a")
                .unwrap()
                .iter()
                .filter(|r| r.active)
                .count(),
            1
        );
    }

    #[test]
    fn migration_backfills_only_active_legacy_revision_and_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite");
        let legacy = Connection::open(&path).unwrap();
        legacy.execute_batch("CREATE TABLE apps(name TEXT PRIMARY KEY,domain TEXT NOT NULL,manifest TEXT NOT NULL,created_at TEXT NOT NULL DEFAULT (datetime('now')));
            CREATE TABLE revisions(id TEXT NOT NULL,app TEXT NOT NULL,artifact_dir TEXT NOT NULL,created_at TEXT NOT NULL DEFAULT (datetime('now')),active INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(app,id));
            INSERT INTO apps(name,domain,manifest) VALUES('a','r2.localhost','name: a');
            INSERT INTO revisions(id,app,artifact_dir,active) VALUES('r1','a','/x/r1',0),('r2','a','/x/r2',1);").unwrap();
        drop(legacy);
        let db = Db::open(&path).unwrap();
        assert_active(&db, "r2");
        assert!(db.activate("a", "r1").is_err());
        assert!(db.previous_revision("a").unwrap().is_none());
        deploy(&db, "r3");
        drop(db);
        let db = Db::open(&path).unwrap();
        db.activate("a", "r2").unwrap();
        assert_active(&db, "r2");
        assert!(db.activate("a", "r1").is_err());
    }
}
