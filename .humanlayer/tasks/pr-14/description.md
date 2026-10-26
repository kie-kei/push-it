## Why the change

The built-in database that deployed apps talk to was SQLite behind a SQL interface, and we want the platform's internal storage to be NoSQL — schema-free key-value documents — so apps stop writing SQL against a hidden schema.

## Special things to note

- No migration: existing `appdata/<app>/local.db` files stay on disk but are unreachable from guests. The platform has no production deployments yet, so this is a deliberate clean cutover, not an oversight.
- Values are opaque bytes — JSON documents are a convention. There is no query language: selection is key prefixes or scanning paginated `list-keys`; heavy relational/analytic work should still use external databases via `wasi:sql`.
- Only the draft's `store` interface is vendored and linked; `atomics`/`batch`/`watch` are deliberately left out until a scenario needs CAS or batching.

## Change outline

The reserved connection name `local` moves from wasi:sql to a new wasi:keyvalue capability. Guests open the bucket by name; the host resolves it to an embedded redb file scoped to the app, so data survives redeploys and rollbacks exactly like the old sqlite did.

```diff
 # pushit.yaml (app manifest)
 capabilities:
-  sql: [local]                   # built-in per-app sqlite
+  kv: [local]                    # built-in per-app key-value store
```

```diff
 # examples/hello/fn/wit/world.wit (guest imports)
-  import wasi:sql/types@0.2.0-draft;
-  import wasi:sql/readwrite@0.2.0-draft;
+  import wasi:keyvalue/store@0.2.0-draft;
```

The guest-facing contract is the upstream draft verbatim, vendored next to the existing sql/blobstore drafts; the host implements it over redb following the same shape as `sql_host`/`blob_host`.

```wit
// wit/kv/store.wit — vendored as-is (wasi:keyvalue@0.2.0-draft)
interface store {
    variant error { no-such-store, access-denied, other(string) }
    record key-response { keys: list<string>, cursor: option<u64> }
    open: func(identifier: string) -> result<bucket, error>;
    resource bucket {
        get:       func(key: string) -> result<option<list<u8>>, error>;
        set:       func(key: string, value: list<u8>) -> result<_, error>;
        delete:    func(key: string) -> result<_, error>;
        exists:    func(key: string) -> result<bool, error>;
        list-keys: func(cursor: option<u64>) -> result<key-response, error>;
    }
}
```

Per-request flow: the manifest grant gates `store.open`, and the bucket handle wraps an `Arc<redb::Database>` cached per file path on the runtime, so concurrent requests share one open database.

```diff
 store.open(name)
-  sql: authorize name → 'local' ? $PUSHIT_DATA/appdata/<app>/local.db (sqlx)
+  kv:  name == "local" and granted by capabilities.kv
+       → $PUSHIT_DATA/appdata/<app>/local.kv (redb, cached Arc<Database>)
 bucket.get/set/delete/exists
-  (n/a)
+  redb table "kv" (<str> → <bytes>), reads snapshot, writes in one txn
 bucket.list-keys(cursor)
+  offset-cursor scan, ≤256 keys per page
```

```text
wit/kv/                     vendored draft (store.wit) + host world
controller/src/
├── kv_host.rs              new: wasi:keyvalue host over redb (+ tests)
├── manifest.rs             capabilities.kv
├── runtime.rs              links kv_host, KvCtx, open-db cache
├── server.rs               passes kv grants, drops dead HostCaps.data_dir
└── sql_host.rs             'local' reserved name removed entirely
examples/hello/             /_fn/kv demo route, sql deps dropped
cli/src/                    dev runtime + bundled skill reference for kv.md
docs/ skills/ scripts/      kv.md contract, e2e suites moved to /_fn/kv
```
