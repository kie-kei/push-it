# SQL capability (wasi:sql)

Components can query databases through host functions instead of raw
sockets. The guest imports `wasi:sql@0.2.0-draft` (`types` + `readwrite`);
the controller resolves *logical* connection names to real databases.
DSNs live in encrypted app-private or platform-wide secrets. The deployer
binds logical names to secret sources (`secret:<app-secret-key>` or
`platform:<global-secret-name>`); manifests contain source references,
not credential values. The SQL host resolves these secrets and connects
on behalf of the component without exposing the DSN to it. For the
built-in NoSQL store see [kv.md](kv.md).

## Contract

WIT package `wasi:sql@0.2.0-draft` (vendored in `wit/sql/`):

```wit
// types.wit
record row { field-name: string, value: data-type }
variant data-type {
    int32(s32), int64(s64), uint32(u32), uint64(u64),
    float(f64), double(f64), str(string), boolean(bool),
    date(string), time(string), timestamp(string),
    binary(list<u8>), null
}
resource statement {
    prepare: static func(query: string, params: list<string>)
        -> result<statement, error>;
}
resource error { trace: func() -> string; }
resource connection {
    open: static func(name: string) -> result<connection, error>;
}

// readwrite.wit
query: func(c: borrow<connection>, q: borrow<statement>)
    -> result<list<row>, error>;
exec:  func(c: borrow<connection>, q: borrow<statement>)
    -> result<u32, error>;
```

`query` returns **flattened** rows: one `row` record per column, so a
3-column result of 10 rows is 30 records. `exec` returns rows affected.
`error.trace()` returns the host error message.

## Granting access

pushit.yaml — the app declares logical connection names, nothing about
where the credentials live:

```yaml
capabilities:
  sql: [db]   # logical names, bound at deploy time
```

Every name must be **bound at deploy time** by the deployer (root
manifest) to a secret source — the app repo never decides the scope:

```yaml
# root pushit.yaml (deploy side)
apps:
  myapp:
    repo: git@…
    env: {}
    sql:
      db: platform:arcadia-prod    # platform-wide secret, shared by apps
      # db: secret:arcadia-dsn     # …or an app-private secret
```

```bash
pushit secret global set arcadia-prod='postgres://user:pass@arcadiamusicalliance.com:5432/arcadia'
# or app-scoped: pushit secret set myapp arcadia-dsn='postgres://…'
```

The guest opens `connection.open("db")`; the controller reads the bound
secret and connects itself — the DSN never reaches the component.
Unbound names fail manifest validation at deploy; bindings pointing at
missing secrets fail deploy with a `pushit secret …` hint.

`connection.open(name)` succeeds only if `name` is in `capabilities.sql`
and a DSN is resolvable for it; otherwise it returns an error resource.
Pools are cached per (app revision, name).

## Drivers

| DSN scheme | Driver | Notes |
|---|---|---|
| `postgres://`, `postgresql://` | sqlx `AnyPool` | `?` rewritten to `$N` |
| `mysql://` | sqlx `AnyPool` | native `?` binds |
| `sqlite://` | sqlx `AnyPool` | file must exist (sqlite `create_if_missing` is off) |
| `clickhouse://`, `ch://` | ClickHouse **HTTP** endpoint (reqwest) | port defaults to 8123; params substituted host-side |

ClickHouse DSNs point at the HTTP interface, not the native TCP port
9000: `clickhouse://[user[:pass]@]host[:8123][/db][?extra=params]`.
Responses are read as `JSONEachRow`.

## Parameters

Statements use `?` placeholders; `params` are strings.

- postgres: `?` → `$1..$N`, real bind params.
- mysql/sqlite: `?` passed through, real bind params.
- ClickHouse: substituted literally as quoted, escaped strings
  (`'` → `\'`, `\` → `\\`). Numbers/dates arrive as CH string literals —
  let CH coerce, or cast in SQL. `?` inside `'...'` literals is not
  treated as a placeholder (and `''` escaping is honoured).

## Type mapping

| data-type | source |
|---|---|
| int32/int64 | SQL integer types |
| uint32/uint64 | unsigned/CH numbers |
| float/double | REAL/FLOAT/DOUBLE/NUMERIC/DECIMAL |
| str | TEXT/VARCHAR/JSON/UUID/unknown |
| boolean | BOOL |
| date/time/timestamp | DATE / TIME / TIMESTAMP (as driver strings) |
| binary | BLOB/BYTEA |
| null | NULL or undecodable |

## Errors

All failures (auth, connect, prepare-check, execute) return an `error`
resource; `error.trace()` yields the message. Connections failed at
`open` are not cached — the next call retries `connect`.

## Example

Any app that declares `capabilities.sql: [db]` (with the deploy-side
binding above) can run the round trip from `examples/hello/fn` history:
`connection.open("db")`, `statement.prepare`, `exec`, `query`. For a
working checkout that needs no external database, see the `/_fn/kv`
route in `examples/hello` ([kv.md](kv.md)) — the built-in store.
