use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::path::Path;
use parking_lot::Mutex;
use std::sync::Arc;

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
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
        let conn = Connection::open(path)?;
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
               PRIMARY KEY(app, key));",
        )?;
        Ok(Db {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn c(&self) -> parking_lot::MutexGuard<'_, Connection> {
        self.conn.lock()
    }

    // ---- apps ----

    pub fn upsert_app(&self, name: &str, domain: &str, manifest: &str) -> Result<()> {
        self.c().execute(
            "INSERT INTO apps(name,domain,manifest) VALUES(?1,?2,?3)
             ON CONFLICT(name) DO UPDATE SET domain=?2, manifest=?3",
            params![name, domain, manifest],
        )?;
        Ok(())
    }

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

    /// Resolve a Host subdomain (route name) to an app.
    pub fn app_by_domain(&self, domain_prefix: &str) -> Result<Option<App>> {
        let app = self
            .c()
            .query_row(
                "SELECT name,domain,manifest,created_at FROM apps
                 WHERE name=?1 OR domain=?1 OR domain LIKE ?1||'.%'
                 LIMIT 1",
                params![domain_prefix],
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
                    status: "ok".into(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn delete_app(&self, name: &str) -> Result<()> {
        let conn = self.c();
        conn.execute("DELETE FROM revisions WHERE app=?1", params![name])?;
        conn.execute("DELETE FROM secrets WHERE app=?1", params![name])?;
        conn.execute("DELETE FROM apps WHERE name=?1", params![name])?;
        Ok(())
    }

    // ---- revisions ----

    pub fn add_revision(&self, app: &str, id: &str, dir: &str, active: bool) -> Result<()> {
        self.c().execute(
            "INSERT INTO revisions(id,app,artifact_dir,active) VALUES(?1,?2,?3,?4)",
            params![id, app, dir, active as i32],
        )?;
        Ok(())
    }

    pub fn activate(&self, app: &str, id: &str) -> Result<()> {
        let conn = self.c();
        let exists: bool = conn.query_row(
            "SELECT COUNT(1)>0 FROM revisions WHERE app=?1 AND id=?2",
            params![app, id],
            |r| r.get(0),
        )?;
        if !exists {
            bail!("no revision '{id}' for app '{app}'");
        }
        conn.execute(
            "UPDATE revisions SET active=0 WHERE app=?1",
            params![app],
        )?;
        conn.execute(
            "UPDATE revisions SET active=1 WHERE app=?1 AND id=?2",
            params![app, id],
        )?;
        Ok(())
    }

    /// Previous revision = newest non-active one.
    pub fn previous_revision(&self, app: &str) -> Result<Option<String>> {
        self.c()
            .query_row(
                "SELECT id FROM revisions WHERE app=?1 AND active=0
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
            "SELECT id,app,artifact_dir,created_at,active FROM revisions
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
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn active_revision(&self, app: &str) -> Result<Option<Revision>> {
        Ok(self
            .c()
            .query_row(
                "SELECT id,app,artifact_dir,created_at,active FROM revisions
                 WHERE app=?1 AND active=1",
                params![app],
                |r| {
                    Ok(Revision {
                        id: r.get(0)?,
                        app: r.get(1)?,
                        artifact_dir: r.get(2)?,
                        created_at: r.get(3)?,
                        active: true,
                    })
                },
            )
            .optional()?)
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
        let mut st =
            conn.prepare("SELECT key FROM secrets WHERE app=?1 ORDER BY key")?;
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
        let mut st =
            conn.prepare("SELECT key, value_enc FROM secrets WHERE app=?1")?;
        let rows = st
            .query_map(params![app], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<(String, Vec<u8>)>>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revision_activate_rollback() {
        let db = Db::open(Path::new(":memory:")).unwrap();
        db.upsert_app("a", "a.localhost", "name: a").unwrap();
        db.add_revision("a", "r1", "/x/r1", true).unwrap();
        db.add_revision("a", "r2", "/x/r2", false).unwrap();
        db.activate("a", "r2").unwrap();
        assert_eq!(db.active_revision("a").unwrap().unwrap().id, "r2");
        assert_eq!(db.previous_revision("a").unwrap().as_deref(), Some("r1"));
        db.activate("a", "r1").unwrap();
        assert_eq!(db.active_revision("a").unwrap().unwrap().id, "r1");
        assert!(db.activate("a", "nope").is_err());
    }
}
