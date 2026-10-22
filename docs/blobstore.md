# Blobstore capability (wasi:blobstore)

Components can store objects through host functions instead of talking to
S3/HTTP themselves. The guest imports `wasi:blobstore@0.2.0-draft`
(vendored in `wit/blobstore/`); the controller resolves *logical* store
names to a backend via `$PUSHIT_DATA/blobstores.yaml`. Credentials never
reach the guest.

## Contract

Interfaces `types`, `container`, `blobstore` of `wasi:blobstore@0.2.0-draft`.

| Guest call | Host behavior |
|---|---|
| `blobstore.get-container(name)` / `create-container(name)` | authorize `name` (below) → `container` resource. Object stores need no explicit creation — `create-container` is an alias for `get-container` |
| `blobstore.delete-container(name)` | recursive delete of the container's key prefix |
| `blobstore.container-exists(name)` | true if any object exists under the prefix; an empty fixed fs root also counts |
| `blobstore.copy-object` / `move-object` | opendal `copy`/`rename` when both ids resolve to the same logical store; read+write(+delete) fallback across stores |
| `container.name` / `info` | authorized name; `created-at` = backend root mtime or 0 |
| `container.get-data(name, start, end)` | range read (`end` inclusive, `u64::MAX` = whole object) → `incoming-value` |
| `container.write-data(name, v)` | buffer collected from `outgoing-value` → `write`; call `v.finish()` after |
| `container.list-objects()` → `stream-object-names` | recursive list, batched cursor (`read-stream-object-names(len)` → `(batch, done)`) |
| `container.delete-object(s)` / `has-object` / `object-info` / `clear` | delete / exists / metadata (`created-at` = last-modified; spec TODO) / recursive delete |

## Granting access

pushit.yaml:

```yaml
capabilities:
  blobstore: [files, media]   # logical names only
```

Platform admin maps names in `$PUSHIT_DATA/blobstores.yaml`:

```yaml
# fixed container: the DSN root IS the container; guests may only use
# the container named after the logical entry ("files")
files: fs:///var/lib/pushit/blobs

# map form: explicit container, or "*" = any guest-chosen name
media:
  url: "s3://prod-media/uploads?endpoint=https://s3.internal&access_key_id=...&secret_access_key=...&region=us-east-1"
  container: "*"        # objects land under uploads/<container>/<object>

# fixed but named differently than the logical key
docs:
  url: "fs:///srv/docs"
  container: "docs"
```

Authorization: `get-container(name)` / `create-container(name)` must be
granted by *some* `capabilities.blobstore` entry whose resolved container
grant equals `name` (or is `*`). For wildcard entries objects are namespaced
`<container>/<object>` inside the store root; for fixed entries the root is
the container.

## DSN drivers

- `fs:///abs/path` — local filesystem (e2e, dev).
- `s3://bucket[/prefix]?endpoint=&access_key_id=&secret_access_key=&region=&session_token=` —
  S3-compatible. Params are percent-decoded; unknown params ignored.

## MVP limits

- Object bodies are fully buffered in memory on both read and write —
  fine for internal apps; objects >100MB should get real streaming
  (marked `shortcut:` in `controller/src/blob_host.rs`).
- `created-at` is last-modified time (object stores don't track creation);
  containers report root mtime or 0. The draft spec has a TODO here.
- `types.error` is a plain string in this draft (no error resource).
- Vendored `wasi:io` is `@0.2.0` (the platform's wasip2 line), not the
  spec's `@0.2.2` — re-tag if the spec's io version is ever required.
