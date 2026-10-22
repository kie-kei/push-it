//! `wasi:blobstore@0.2.0-draft` host functions.
//!
//! Components address a *logical* store granted via `capabilities.blobstore`
//! in pushit.yaml; the host resolves it from `$PUSHIT_DATA/blobstores.yaml`.
//! Credentials never reach the guest.
//!
//! blobstores.yaml entries (logical name → target):
//!   files: "fs:///abs/path"                # container fixed to "files"
//!   media: { url: "s3://bucket/prefix?endpoint=...", container: "*" }
//! `container: "*"` lets the guest name containers; objects then live under
//! `<prefix>/<container>/<object>`. Otherwise the DSN root *is* the container
//! and `get-container(name)` only allows `name == <logical>` (or an explicit
//! `container:` value in the entry).
//!
//! MVP: object bodies are buffered whole in memory on both read and write.
//! shortcut: whole-object buffering, real streaming when >100MB objects
//! are needed — pipe `MemoryInputPipe`/`MemoryOutputPipe` to opendal's
//! reader/writer then.
//!
//! `created-at` metadata: the draft spec wants creation time; object stores
//! only give last-modified (opendal `Timestamp`), so that is what we return.
//! Same for containers: fs mtime, else 0.

use anyhow::{bail, Context as _, Result};
use opendal::Operator;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use wasmtime::component::Resource;
use wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe};
use wasmtime_wasi::p2::IoView;

mod bindings {
    wasmtime::component::bindgen!({
        path: "../wit/blobstore",
        world: "wasi:blobstore/blobstore-host",
        async: true,
        with: {
            "wasi:io/streams/input-stream": wasmtime_wasi::p2::DynInputStream,
            "wasi:io/streams/output-stream": wasmtime_wasi::p2::DynOutputStream,
            "wasi:blobstore/types/outgoing-value": super::OutgoingValue,
            "wasi:blobstore/types/incoming-value": super::IncomingValue,
            "wasi:blobstore/container/container": super::Container,
            "wasi:blobstore/container/stream-object-names": super::StreamObjectNames,
        },
    });
}
use wasmtime::component::HasSelf;

use bindings::wasi::blobstore::types::{
    ContainerMetadata, ObjectId, ObjectMetadata,
};

use crate::runtime::ComponentCtx;

/// Host rep of `wasi:blobstore/types.outgoing-value`: a body being written.
pub struct OutgoingValue {
    pipe: MemoryOutputPipe,
}

/// Host rep of `wasi:blobstore/types.incoming-value`: a fully-read object.
pub struct IncomingValue {
    bytes: Vec<u8>,
}

/// Host rep of `wasi:blobstore/container.stream-object-names`.
pub struct StreamObjectNames {
    names: Vec<String>,
    pos: usize,
}

/// Host rep of `wasi:blobstore/container.container`.
pub struct Container {
    op: opendal::Operator,
    /// Logical container name as the guest sees it.
    name: String,
    /// Key prefix inside the operator — "" unless the entry is wildcard.
    prefix: String,
}

/// One blobstores.yaml entry.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum BlobEntry {
    /// `name: dsn` — container fixed to the logical name.
    Dsn(String),
    /// Extended form.
    Map {
        url: String,
        /// Container the guest may use; `*` = any name.
        container: Option<String>,
    },
}

/// Everything a component invocation needs for wasi:blobstore.
pub struct BlobCtx {
    /// Names granted by the manifest (`capabilities.blobstore`).
    pub allowed: Vec<String>,
    /// logical name → entry, loaded from `$PUSHIT_DATA/blobstores.yaml`.
    pub stores: HashMap<String, BlobEntry>,
}

/// name → entry from `<data_dir>/blobstores.yaml`; absent file → empty map.
pub fn load_blobstores(data_dir: &Path) -> Result<HashMap<String, BlobEntry>> {
    let path = data_dir.join("blobstores.yaml");
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_yaml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

impl BlobEntry {
    fn dsn(&self) -> &str {
        match self {
            BlobEntry::Dsn(d) => d,
            BlobEntry::Map { url, .. } => url,
        }
    }
}

/// Resolve `name` against the manifest grant + blobstores.yaml; on success
/// returns (operator, authorized container for `want`, key prefix).
fn authorize<'m>(
    name: &str,
    want: &str,
    allowed: &'m [String],
    stores: &'m HashMap<String, BlobEntry>,
) -> Result<Resolved, String> {
    if !allowed.iter().any(|a| a == name) {
        return Err(format!("blobstore '{name}' not granted by capabilities.blobstore"));
    }
    let entry = stores
        .get(name)
        .ok_or_else(|| format!("blobstore '{name}' missing from blobstores.yaml"))?;
    let grant = match entry {
        BlobEntry::Dsn(_) => name.to_string(),
        BlobEntry::Map { container, .. } => container.clone().unwrap_or_else(|| name.to_string()),
    };
    if grant != "*" && want != grant {
        return Err(format!(
            "container '{want}' not allowed for blobstore '{name}' (granted: '{grant}')"
        ));
    }
    let op = operator(entry.dsn()).map_err(|e| format!("{e:#}"))?;
    // Wildcard entries namespace objects under <container>/.
    let prefix = if grant == "*" {
        format!("{want}/")
    } else {
        String::new()
    };
    Ok(Resolved {
        logical: name.to_string(),
        op,
        container: want.to_string(),
        prefix,
    })
}

#[derive(Debug)]
struct Resolved {
    /// logical entry name — same logical ⇒ same operator ⇒ native copy/rename.
    logical: String,
    op: opendal::Operator,
    container: String,
    prefix: String,
}

/// `fs:///abs/path` or `s3://bucket/prefix?param=...` → opendal Operator.
/// Unknown query params are passed through as builder config where the
/// backend supports them; unrecognized keys are ignored (permissive DSN).
fn operator(dsn: &str) -> Result<Operator> {
    let (scheme, rest) = dsn
        .split_once("://")
        .context("blobstore DSN missing scheme")?;
    match scheme {
        "fs" => {
            let path = rest.split('?').next().unwrap_or("");
            Ok(Operator::new(opendal::services::Fs::default().root(path))?)
        }
        "s3" => {
            let (authority, query) = rest.split_once('?').unwrap_or((rest, ""));
            let (bucket, root) = authority.split_once('/').unwrap_or((authority, ""));
            if bucket.is_empty() {
                bail!("s3 DSN missing bucket");
            }
            let mut b = opendal::services::S3::default().bucket(bucket);
            if !root.is_empty() {
                b = b.root(&format!("/{root}"));
            }
            for (k, v) in parse_query(query) {
                match k.as_str() {
                    "endpoint" => {
                        b = b.endpoint(&v);
                    }
                    "access_key_id" => {
                        b = b.access_key_id(&v);
                    }
                    "secret_access_key" => {
                        b = b.secret_access_key(&v);
                    }
                    "region" => {
                        b = b.region(&v);
                    }
                    "session_token" => {
                        b = b.session_token(&v);
                    }
                    _ => {} // permissive: ignore unknown params
                }
            }
            Ok(Operator::new(b)?)
        }
        other => bail!("unsupported blobstore scheme '{other}'"),
    }
}

/// `a=b&c=d` (percent-decoded) → pairs.
fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|s| !s.is_empty())
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((pct(k), pct(v)))
        })
        .collect()
}

/// Minimal percent-decoding for DSN params.
fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(h) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(h as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn now_or(meta: &opendal::Metadata) -> u64 {
    meta.last_modified()
        .map(|t| t.into_inner().as_second() as u64)
        .unwrap_or(0)
}

fn key(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}{name}")
    }
}

// ---------- wasi:blobstore/types ----------

impl bindings::wasi::blobstore::types::Host for ComponentCtx {}

impl bindings::wasi::blobstore::types::HostOutgoingValue for ComponentCtx {
    async fn new_outgoing_value(&mut self) -> Resource<OutgoingValue> {
        let v = OutgoingValue {
            pipe: MemoryOutputPipe::new(usize::MAX),
        };
        self.table().push(v).unwrap()
    }

    async fn outgoing_value_write_body(
        &mut self,
        self_: Resource<OutgoingValue>,
    ) -> Result<Resource<wasmtime_wasi::p2::DynOutputStream>, ()> {
        let v = self.table().get(&self_).map_err(|_| ())?;
        let s: wasmtime_wasi::p2::DynOutputStream = Box::new(v.pipe.clone());
        self.table()
            .push(s)
            .map_err(|_| ())
    }

    async fn finish(&mut self, this: Resource<OutgoingValue>) -> Result<(), String> {
        // The body is already fully buffered; nothing to flush. Drop rep.
        self.table().delete(this).map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn drop(&mut self, rep: Resource<OutgoingValue>) -> Result<()> {
        self.table().delete(rep)?;
        Ok(())
    }
}

impl bindings::wasi::blobstore::types::HostIncomingValue for ComponentCtx {
    async fn incoming_value_consume_sync(
        &mut self,
        this: Resource<IncomingValue>,
    ) -> Result<Vec<u8>, String> {
        let v = self.table().delete(this).map_err(|e| e.to_string())?;
        Ok(v.bytes)
    }

    async fn incoming_value_consume_async(
        &mut self,
        this: Resource<IncomingValue>,
    ) -> Result<Resource<wasmtime_wasi::p2::DynInputStream>, String> {
        let v = self.table().delete(this).map_err(|e| e.to_string())?;
        let s: wasmtime_wasi::p2::DynInputStream =
            Box::new(MemoryInputPipe::new(v.bytes));
        self.table().push(s).map_err(|e| e.to_string())
    }

    async fn size(&mut self, self_: Resource<IncomingValue>) -> u64 {
        self.table()
            .get(&self_)
            .map(|v: &IncomingValue| v.bytes.len() as u64)
            .unwrap_or(0)
    }

    async fn drop(&mut self, rep: Resource<IncomingValue>) -> Result<()> {
        self.table().delete(rep)?;
        Ok(())
    }
}

// ---------- wasi:blobstore/container ----------

impl bindings::wasi::blobstore::container::Host for ComponentCtx {}

impl bindings::wasi::blobstore::container::HostContainer for ComponentCtx {
    async fn name(&mut self, self_: Resource<Container>) -> Result<String, String> {
        self.table()
            .get(&self_)
            .map(|c: &Container| c.name.clone())
            .map_err(|e| e.to_string())
    }

    async fn info(&mut self, self_: Resource<Container>) -> Result<ContainerMetadata, String> {
        let c = self.table().get(&self_).map_err(|e| e.to_string())?;
        // Container create time isn't tracked; use root mtime if the backend
        // reports it, else 0.
        let created = match c.op.stat(&c.prefix).await {
            Ok(m) => now_or(&m),
            Err(_) => 0,
        };
        Ok(ContainerMetadata {
            name: c.name.clone(),
            created_at: created,
        })
    }

    async fn get_data(
        &mut self,
        self_: Resource<Container>,
        name: String,
        start: u64,
        end: u64,
    ) -> Result<Resource<IncomingValue>, String> {
        let c = self.table().get(&self_).map_err(|e| e.to_string())?;
        let path = key(&c.prefix, &name);
        let read = if end == u64::MAX {
            c.op.read(&path).await.map(|b| b.to_vec())
        } else {
            c.op
                .read_with(&path)
                .range(start..=end.min(start.saturating_add(u64::MAX - 1)))
                .await
                .map(|b| b.to_vec())
        };
        let bytes = read.map_err(|e| e.to_string())?;
        self.table()
            .push(IncomingValue { bytes })
            .map_err(|e| e.to_string())
    }

    async fn write_data(
        &mut self,
        self_: Resource<Container>,
        name: String,
        data: Resource<OutgoingValue>,
    ) -> Result<(), String> {
        let out = self
            .table()
            .get(&data)
            .map(|v: &OutgoingValue| v.pipe.contents().to_vec())
            .map_err(|e| e.to_string())?;
        let c = self.table().get(&self_).map_err(|e| e.to_string())?;
        c.op.write(&key(&c.prefix, &name), out)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn list_objects(
        &mut self,
        self_: Resource<Container>,
    ) -> Result<Resource<StreamObjectNames>, String> {
        let c = self.table().get(&self_).map_err(|e| e.to_string())?;
        let mut l = c
            .op
            .lister_with(&c.prefix)
            .recursive(true)
            .await
            .map_err(|e| e.to_string())?;
        let mut names = Vec::new();
        use futures::TryStreamExt;
        while let Some(e) = l.try_next().await.map_err(|e| e.to_string())? {
            let p = e.path();
            // skip directory placeholders and the prefix root itself
            if p.ends_with('/') || p == c.prefix {
                continue;
            }
            names.push(
                p.strip_prefix(&c.prefix).unwrap_or(p).to_string(),
            );
        }
        self.table()
            .push(StreamObjectNames { names, pos: 0 })
            .map_err(|e| e.to_string())
    }

    async fn delete_object(&mut self, self_: Resource<Container>, name: String) -> Result<(), String> {
        let c = self.table().get(&self_).map_err(|e| e.to_string())?;
        c.op.delete(&key(&c.prefix, &name))
            .await
            .map_err(|e| e.to_string())
    }

    async fn delete_objects(
        &mut self,
        self_: Resource<Container>,
        names: Vec<String>,
    ) -> Result<(), String> {
        let c = self.table().get(&self_).map_err(|e| e.to_string())?;
        for n in &names {
            c.op
                .delete(&key(&c.prefix, n))
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    async fn has_object(&mut self, self_: Resource<Container>, name: String) -> Result<bool, String> {
        let c = self.table().get(&self_).map_err(|e| e.to_string())?;
        c.op.exists(&key(&c.prefix, &name))
            .await
            .map_err(|e| e.to_string())
    }

    async fn object_info(
        &mut self,
        self_: Resource<Container>,
        name: String,
    ) -> Result<ObjectMetadata, String> {
        let c = self.table().get(&self_).map_err(|e| e.to_string())?;
        let m = c
            .op
            .stat(&key(&c.prefix, &name))
            .await
            .map_err(|e| e.to_string())?;
        Ok(ObjectMetadata {
            name,
            container: c.name.clone(),
            created_at: now_or(&m),
            size: m.content_length(),
        })
    }

    async fn clear(&mut self, self_: Resource<Container>) -> Result<(), String> {
        let c = self.table().get(&self_).map_err(|e| e.to_string())?;
        c.op
            .delete_with(&c.prefix)
            .recursive(true)
            .await
            .map_err(|e| e.to_string())
    }

    async fn drop(&mut self, rep: Resource<Container>) -> Result<()> {
        self.table().delete(rep)?;
        Ok(())
    }
}

impl bindings::wasi::blobstore::container::HostStreamObjectNames for ComponentCtx {
    async fn read_stream_object_names(
        &mut self,
        self_: Resource<StreamObjectNames>,
        len: u64,
    ) -> Result<(Vec<String>, bool), String> {
        let s = self
            .table()
            .get_mut(&self_)
            .map_err(|e| e.to_string())?;
        let s: &mut StreamObjectNames = s;
        let end = (s.pos + len as usize).min(s.names.len());
        let batch = s.names[s.pos..end].to_vec();
        s.pos = end;
        Ok((batch, s.pos >= s.names.len()))
    }

    async fn skip_stream_object_names(
        &mut self,
        self_: Resource<StreamObjectNames>,
        num: u64,
    ) -> Result<(u64, bool), String> {
        let s = self
            .table()
            .get_mut(&self_)
            .map_err(|e| e.to_string())?;
        let s: &mut StreamObjectNames = s;
        let skipped = num.min((s.names.len() - s.pos) as u64);
        s.pos += skipped as usize;
        Ok((skipped, s.pos >= s.names.len()))
    }

    async fn drop(&mut self, rep: Resource<StreamObjectNames>) -> Result<()> {
        self.table().delete(rep)?;
        Ok(())
    }
}

// ---------- wasi:blobstore/blobstore ----------

impl bindings::wasi::blobstore::blobstore::Host for ComponentCtx {
    async fn create_container(&mut self, name: String) -> Result<Resource<Container>, String> {
        // Object stores need no explicit container creation; fs makes dirs
        // lazily on write. Authorize and hand back the handle.
        self.get_container(name).await
    }

    async fn get_container(&mut self, name: String) -> Result<Resource<Container>, String> {
        let c = self.open_container(&name)?;
        self.table().push(c).map_err(|e| e.to_string())
    }

    async fn delete_container(&mut self, name: String) -> Result<(), String> {
        let c = self.open_container(&name)?;
        c.op
            .delete_with(&c.prefix)
            .recursive(true)
            .await
            .map_err(|e| e.to_string())
    }

    async fn container_exists(&mut self, name: String) -> Result<bool, String> {
        // An authorized name "exists" if the backend holds any object under
        // its prefix; an empty fs root still counts for fixed entries.
        let c = match self.open_container(&name) {
            Ok(c) => c,
            Err(_) => return Ok(false),
        };
        let mut l = c
            .op
            .lister_with(&c.prefix)
            .recursive(true)
            .await
            .map_err(|e| e.to_string())?;
        use futures::TryStreamExt;
        while let Some(e) = l.try_next().await.map_err(|e| e.to_string())? {
            if !e.path().ends_with('/') && e.path() != c.prefix {
                return Ok(true);
            }
        }
        Ok(c.prefix.is_empty() && c.op.stat("").await.is_ok())
    }

    async fn copy_object(&mut self, src: ObjectId, dest: ObjectId) -> Result<(), String> {
        let s = self.resolve_id(&src)?;
        let d = self.resolve_id(&dest)?;
        let (sp, dp) = (key(&s.prefix, &src.object), key(&d.prefix, &dest.object));
        if s.logical == d.logical {
            return s.op.copy(&sp, &dp).await.map(|_| ()).map_err(|e| e.to_string());
        }
        // cross-store: read + write fallback
        let b = s.op.read(&sp).await.map_err(|e| e.to_string())?;
        d.op.write(&dp, b.to_vec()).await.map(|_| ()).map_err(|e| e.to_string())
    }

    async fn move_object(&mut self, src: ObjectId, dest: ObjectId) -> Result<(), String> {
        let s = self.resolve_id(&src)?;
        let d = self.resolve_id(&dest)?;
        let (sp, dp) = (key(&s.prefix, &src.object), key(&d.prefix, &dest.object));
        if s.logical == d.logical {
            return s.op.rename(&sp, &dp).await.map(|_| ()).map_err(|e| e.to_string());
        }
        let b = s.op.read(&sp).await.map_err(|e| e.to_string())?;
        d.op.write(&dp, b.to_vec()).await.map_err(|e| e.to_string())?;
        s.op.delete(&sp).await.map(|_| ()).map_err(|e| e.to_string())
    }

}

impl ComponentCtx {
    /// Open the guest-named container: find the granted logical entry that
    /// authorizes `name` and resolve it to a `Container`.
    fn open_container(&mut self, name: &str) -> Result<Container, String> {
        for l in self.blob.allowed.clone() {
            if let Ok(r) = authorize(&l, name, &self.blob.allowed, &self.blob.stores) {
                return Ok(Container {
                    op: r.op,
                    name: r.container,
                    prefix: r.prefix,
                });
            }
        }
        Err(format!("container '{name}' not granted by capabilities.blobstore"))
    }

    /// Resolve an object-id to its backend (operator + prefix + logical name).
    fn resolve_id(&mut self, id: &ObjectId) -> Result<Resolved, String> {
        for l in self.blob.allowed.clone() {
            if let Ok(r) = authorize(&l, &id.container, &self.blob.allowed, &self.blob.stores) {
                return Ok(r);
            }
        }
        Err(format!("container '{}' not granted", id.container))
    }
}

/// Register wasi:blobstore imports on the linker.
pub fn add_to_linker(linker: &mut wasmtime::component::Linker<ComponentCtx>) -> Result<()> {
    bindings::wasi::blobstore::types::add_to_linker::<_, HasSelf<_>>(linker, |d| d)?;
    bindings::wasi::blobstore::container::add_to_linker::<_, HasSelf<_>>(linker, |d| d)?;
    bindings::wasi::blobstore::blobstore::add_to_linker::<_, HasSelf<_>>(linker, |d| d)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stores(yaml: &str) -> HashMap<String, BlobEntry> {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn authorize_fixed_container() {
        let s = stores("files: fs:///tmp/x\n");
        let allowed = vec!["files".to_string()];
        assert!(authorize("files", "files", &allowed, &s).is_ok());
        let e = authorize("files", "other", &allowed, &s).unwrap_err();
        assert!(e.contains("not allowed"), "{e}");
    }

    #[test]
    fn authorize_requires_grant_and_entry() {
        let s = stores("files: fs:///tmp/x\n");
        assert!(authorize("files", "files", &[], &s)
            .unwrap_err()
            .contains("capabilities"));
        let allowed = vec!["ghost".to_string()];
        assert!(authorize("ghost", "ghost", &allowed, &s)
            .unwrap_err()
            .contains("missing"));
    }

    #[test]
    fn authorize_wildcard() {
        let s = stores("media: {url: fs:///tmp/y, container: \"*\"}\n");
        let allowed = vec!["media".to_string()];
        let r = authorize("media", "anything", &allowed, &s).unwrap();
        assert_eq!(r.prefix, "anything/");
        assert_eq!(r.container, "anything");
    }

    #[test]
    fn parse_fs_dsn() {
        let _op = operator("fs:///tmp/pushit-test").unwrap();
    }

    #[test]
    fn parse_s3_dsn() {
        let op = operator(
            "s3://bucket/prefix?endpoint=http://localhost:9000&access_key_id=a&secret_access_key=b&region=us-east-1",
        )
        .unwrap();
        let info = op.info();
        assert_eq!(info.root(), "/prefix/");
    }

    #[test]
    fn reject_bad_scheme() {
        assert!(operator("ftp://x").is_err());
        assert!(operator("no-scheme").is_err());
    }

    #[test]
    fn pct_decoding() {
        assert_eq!(pct("a%20b%2Fc"), "a b/c");
        assert_eq!(pct("plain"), "plain");
    }

    #[tokio::test]
    async fn fs_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let dsn = format!("fs://{}", dir.path().display());
        let op = operator(&dsn).unwrap();
        op.write("a/b.txt", "hi".to_string()).await.unwrap();
        let v = op
            .read_with("a/b.txt")
            .range(0u64..=0)
            .await
            .unwrap()
            .to_vec();
        assert_eq!(v, b"h");
        let m = op.stat("a/b.txt").await.unwrap();
        assert_eq!(m.content_length(), 2);
        op.delete_with("a").recursive(true).await.unwrap();
        assert!(!op.exists("a/b.txt").await.unwrap());
    }
}
