//! `wasi:keyvalue@0.2.0-draft` (store interface) host functions.
//!
//! The reserved bucket name `local` is the app-scoped NoSQL database: an
//! embedded redb key-value store at `$PUSHIT_DATA/appdata/<app>/local.kv`,
//! granted via `capabilities.kv` in pushit.yaml. Values are raw bytes —
//! JSON documents are a convention, not a host format. Data survives
//! redeploys and rollbacks (appdata is outside revision artifacts).
//!
//! `store.open` accepts only `local`; there are no external kv bindings
//! yet — other names fail the same way unbound sql connections do.
//! Atomics/batch/watch interfaces of the draft are not vendored/linked —
//! add them when a scenario needs CAS/increment.
//!
//! shortcut: redb calls block the executor for the duration of a file
//! operation (commit fsyncs); move to `spawn_blocking` if app payloads
//! ever grow past a few MB per write.

use anyhow::Result;
use parking_lot::Mutex;
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use wasmtime::component::{HasSelf, Resource};
use wasmtime_wasi::p2::IoView;

mod bindings {
    wasmtime::component::bindgen!({
        path: "../wit/kv",
        world: "wasi:keyvalue/kv-host",
        async: true,
        with: {
            "wasi:keyvalue/store/bucket": super::Bucket,
        },
    });
}

use crate::runtime::ComponentCtx;
use bindings::wasi::keyvalue::store::{Error, KeyResponse};

const TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("kv");

/// Reserved bucket: the app-scoped embedded store.
const LOCAL: &str = "local";

/// list-keys page size; the guest pages on via the returned cursor.
const PAGE: u64 = 256;

/// Host rep of the `bucket` resource.
pub struct Bucket {
    db: Arc<Database>,
}

/// Open-database cache shared with `Runtime`, keyed by file path. Bounded by
/// the app count; a deleted app's appdata is removed on disk while a stale
/// handle keeps writing to the unlinked inode — harmless.
pub type DbCache = Arc<Mutex<HashMap<PathBuf, Arc<Database>>>>;

/// Path of the app-scoped kv store.
pub fn local_kv_path(data_dir: &Path, app: &str) -> PathBuf {
    data_dir.join("appdata").join(app).join("local.kv")
}

/// Open (creating on first use) or fetch the cached database. `create` never
/// truncates existing app data on reopen.
fn open_db(cache: &DbCache, path: &Path) -> Result<Arc<Database>> {
    // Held across open: concurrent first requests for one app must not race
    // two `Database::create` calls on the same file.
    let mut m = cache.lock();
    if let Some(db) = m.get(path) {
        return Ok(db.clone());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let db = Arc::new(Database::create(path)?);
    // The table must always exist: reads on a never-written store return
    // empty, not TableDoesNotExist.
    let txn = db.begin_write()?;
    txn.open_table(TABLE)?;
    txn.commit()?;
    Ok(m.entry(path.to_path_buf()).or_insert(db).clone())
}

fn e2e(e: impl std::fmt::Display) -> Error {
    Error::Other(e.to_string())
}

/// Everything a component invocation needs for wasi:keyvalue.
pub struct KvCtx {
    /// Names granted by the manifest (`capabilities.kv`); only `local` is
    /// meaningful today.
    pub allowed: Vec<String>,
    /// Data dir — for the `local` store path.
    pub data_dir: PathBuf,
    /// App name — for the `local` store path.
    pub app_name: String,
    /// Shared with `Runtime`: file path → open database.
    pub(crate) dbs: DbCache,
}

impl bindings::wasi::keyvalue::store::Host for ComponentCtx {
    async fn open(&mut self, identifier: String) -> Result<Resource<Bucket>, Error> {
        if identifier != LOCAL || !self.kv.allowed.iter().any(|a| a == LOCAL) {
            return Err(e2e(format!(
                "bucket '{identifier}' not granted by capabilities.kv"
            )));
        }
        let path = local_kv_path(&self.kv.data_dir, &self.kv.app_name);
        let db = open_db(&self.kv.dbs, &path).map_err(|e| e2e(format!("{e:#}")))?;
        self.table()
            .push(Bucket { db })
            .map_err(|e| e2e(format!("{e}")))
    }
}

impl bindings::wasi::keyvalue::store::HostBucket for ComponentCtx {
    async fn get(&mut self, b: Resource<Bucket>, key: String) -> Result<Option<Vec<u8>>, Error> {
        let db = self
            .table()
            .get(&b)
            .map_err(|e| e2e(format!("bad bucket: {e}")))?
            .db
            .clone();
        let t = db
            .begin_read()
            .map_err(e2e)?
            .open_table(TABLE)
            .map_err(e2e)?;
        Ok(t.get(key.as_str()).map_err(e2e)?.map(|v| v.value().to_vec()))
    }

    async fn set(&mut self, b: Resource<Bucket>, key: String, value: Vec<u8>) -> Result<(), Error> {
        let db = self
            .table()
            .get(&b)
            .map_err(|e| e2e(format!("bad bucket: {e}")))?
            .db
            .clone();
        let txn = db.begin_write().map_err(e2e)?;
        {
            let mut t = txn.open_table(TABLE).map_err(e2e)?;
            t.insert(key.as_str(), value.as_slice()).map_err(e2e)?;
        }
        txn.commit().map_err(e2e)?;
        Ok(())
    }

    async fn delete(&mut self, b: Resource<Bucket>, key: String) -> Result<(), Error> {
        let db = self
            .table()
            .get(&b)
            .map_err(|e| e2e(format!("bad bucket: {e}")))?
            .db
            .clone();
        let txn = db.begin_write().map_err(e2e)?;
        {
            let mut t = txn.open_table(TABLE).map_err(e2e)?;
            let _ = t.remove(key.as_str()).map_err(e2e)?;
        }
        txn.commit().map_err(e2e)?;
        Ok(())
    }

    async fn exists(&mut self, b: Resource<Bucket>, key: String) -> Result<bool, Error> {
        let db = self
            .table()
            .get(&b)
            .map_err(|e| e2e(format!("bad bucket: {e}")))?
            .db
            .clone();
        let t = db
            .begin_read()
            .map_err(e2e)?
            .open_table(TABLE)
            .map_err(e2e)?;
        Ok(t.get(key.as_str()).map_err(e2e)?.is_some())
    }

    async fn list_keys(
        &mut self,
        b: Resource<Bucket>,
        cursor: Option<u64>,
    ) -> Result<KeyResponse, Error> {
        let db = self
            .table()
            .get(&b)
            .map_err(|e| e2e(format!("bad bucket: {e}")))?
            .db
            .clone();
        let t = db
            .begin_read()
            .map_err(e2e)?
            .open_table(TABLE)
            .map_err(e2e)?;
        // Offset-style cursor: the draft only promises "MAY show an
        // out-of-date list", so a rescan from the start is spec-legal.
        let start = cursor.unwrap_or(0);
        let mut keys: Vec<String> = t
            .iter()
            .map_err(e2e)?
            .skip(start as usize)
            .take(PAGE as usize + 1)
            .map(|row| row.map(|r| r.0.value().to_string()))
            .collect::<Result<_, _>>()
            .map_err(e2e)?;
        let more = keys.len() > PAGE as usize;
        keys.truncate(PAGE as usize);
        Ok(KeyResponse {
            keys,
            cursor: more.then_some(start + PAGE),
        })
    }

    async fn drop(&mut self, rep: Resource<Bucket>) -> anyhow::Result<()> {
        self.table().delete(rep)?;
        Ok(())
    }
}

/// Register wasi:keyvalue imports on the linker.
pub fn add_to_linker(linker: &mut wasmtime::component::Linker<ComponentCtx>) -> Result<()> {
    bindings::wasi::keyvalue::store::add_to_linker::<_, HasSelf<_>>(linker, |d| d)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(dir: &Path) -> Arc<Database> {
        let cache: DbCache = Arc::new(Mutex::new(HashMap::new()));
        open_db(&cache, &local_kv_path(dir, "myapp")).unwrap()
    }

    fn set(db: &Database, k: &str, v: &[u8]) {
        let txn = db.begin_write().unwrap();
        txn.open_table(TABLE).unwrap().insert(k, v).unwrap();
        txn.commit().unwrap();
    }

    fn get(db: &Database, k: &str) -> Option<Vec<u8>> {
        db.begin_read()
            .unwrap()
            .open_table(TABLE)
            .unwrap()
            .get(k)
            .unwrap()
            .map(|v| v.value().to_vec())
    }

    #[test]
    fn local_kv_path_layout() {
        assert_eq!(
            local_kv_path(Path::new("/data"), "hello"),
            Path::new("/data/appdata/hello/local.kv")
        );
    }

    #[test]
    fn roundtrip_overwrite_delete() {
        let dir = tempfile::tempdir().unwrap();
        let db = db(dir.path());
        assert_eq!(get(&db, "a"), None);
        set(&db, "a", b"1");
        set(&db, "b", b"2");
        set(&db, "a", b"overwritten");
        assert_eq!(get(&db, "a").as_deref(), Some(b"overwritten".as_slice()));
        assert_eq!(get(&db, "b").as_deref(), Some(b"2".as_slice()));
        let txn = db.begin_write().unwrap();
        txn.open_table(TABLE).unwrap().remove("a").unwrap();
        txn.commit().unwrap();
        assert_eq!(get(&db, "a"), None);
    }

    #[test]
    fn reopening_preserves_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = local_kv_path(dir.path(), "myapp");
        set(&db(dir.path()), "k", b"retained");
        // Fresh cache = process restart / another revision of the same app.
        let reopened = open_db(&Arc::new(Mutex::new(HashMap::new())), &path).unwrap();
        assert_eq!(get(&reopened, "k").as_deref(), Some(b"retained".as_slice()));
    }

    #[test]
    fn list_keys_pagination() {
        let dir = tempfile::tempdir().unwrap();
        let db = db(dir.path());
        for i in 0..300 {
            set(&db, &format!("k{i:03}"), b"v");
        }
        let keys = |cursor: Option<u64>| -> (Vec<String>, Option<u64>) {
            let t = db.begin_read().unwrap().open_table(TABLE).unwrap();
            let start = cursor.unwrap_or(0);
            let mut keys: Vec<String> = t
                .iter()
                .unwrap()
                .skip(start as usize)
                .take(PAGE as usize + 1)
                .map(|r| r.map(|r| r.0.value().to_string()))
                .collect::<Result<_, _>>()
                .unwrap();
            let more = keys.len() > PAGE as usize;
            keys.truncate(PAGE as usize);
            (keys, more.then_some(start + PAGE))
        };
        let (first, cur) = keys(None);
        assert_eq!(first.len(), 256);
        assert_eq!(first[0], "k000");
        assert_eq!(cur, Some(256));
        let (second, cur) = keys(cur);
        assert_eq!(second.len(), 44);
        assert_eq!(second[0], "k256");
        assert_eq!(second.last().unwrap(), "k299");
        assert_eq!(cur, None);
    }
}
