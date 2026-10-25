> Reference for app developers. See also `pushit skill` output.

# KV capability (wasi:keyvalue)

Components store structured state through host functions instead of raw
sockets. The guest imports `wasi:keyvalue@0.2.0-draft` (`store`); the
controller maps the reserved bucket name `local` to the app-scoped
embedded key-value store. The internal database is NoSQL: keys map to
opaque byte values — there is no schema and no query language. JSON
documents (one document per key, or a JSON array per collection key) are
a convention between the app and itself; the host never inspects values.

## Contract

WIT package `wasi:keyvalue@0.2.0-draft` (vendored in `wit/kv/`):

```wit
interface store {
    variant error { no-such-store, access-denied, other(string) }
    record key-response {
        keys: list<string>,
        cursor: option<u64>,
    }
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

The draft's `atomics`, `batch` and `watch` interfaces are not linked —
add them when a scenario needs CAS/increment.

## Granting access

pushit.yaml:

```yaml
capabilities:
  kv: [local]   # 'local' = built-in per-app key-value store
```

`local` — reserved name: app-scoped redb database at
`$PUSHIT_DATA/appdata/<app>/local.kv`, no configuration needed. Values
survive redeploys and rollbacks (appdata is outside revision artifacts);
rollback does not roll back kv data. There are no external kv bindings
yet; any other name fails with `not granted by capabilities.kv`.

## Semantics

| Call | Behavior |
|---|---|
| `store.open("local")` | Fails unless `local` is in `capabilities.kv`; returns a bucket handle |
| `bucket.get(key)` | `none` for a missing key — reads on a fresh store work before any write |
| `bucket.set(key, value)` | Upsert; overwrites existing values |
| `bucket.delete(key)` | No-op for a missing key |
| `bucket.exists(key)` | True iff the key exists |
| `bucket.list-keys(cursor)` | Paginated key listing, ≤256 keys per page; pass back `key-response.cursor` until it is `none`. Order is unspecified and pages may be stale under concurrent writes (per the draft) |

Errors are the `store.error` variant; `other(message)` carries the host
message (I/O, storage errors).

## Queries

There are none — that is the point of NoSQL here. Selection by field is
the app's job: keep an index convention (e.g. `user:<id>:doc` keys) or
scan `list-keys` pages and fetch candidates. For heavy relational or
analytic needs use `wasi:sql` with an external database bound at deploy
time — the built-in store is intentionally not a query engine.

## Example

`examples/hello` declares `capabilities.kv: [local]`; its `/_fn/kv`
route calls `store.open("local")`, then sets, gets, lists and deletes a
key over the app-scoped store. No binding or secret is needed.
