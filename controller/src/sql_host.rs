//! `wasi:sql@0.2.0-draft` host functions.
//!
//! Components open a logical connection name (granted via
//! `capabilities.sql` in pushit.yaml); the host resolves it to a real DSN:
//! `local` → built-in app sqlite, `$SECRET/<name>` → app secret, whose
//! value is the DSN. Credentials never reach the guest.
//!
//! Drivers:
//!   postgres|mysql|sqlite → sqlx `AnyPool`
//!   clickhouse            → ClickHouse HTTP endpoint via reqwest
//!     (`clickhouse://[user[:pass]@]host[:port][/db][?param=...]`, port
//!     defaults to 8123 — the HTTP interface, not the native one)
//!
//! Params: guest statements use `?` placeholders. For postgres they are
//! rewritten to `$N`; mysql/sqlite keep `?` (sqlx `Any` binds natively).
//! shortcut: ClickHouse has no bound params over HTTP — params are
//! substituted host-side as escaped string literals; switch to the native
//! `clickhouse` crate if typed params are ever needed.

use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use sqlx::Column;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use wasmtime::component::Resource;

mod bindings {
    wasmtime::component::bindgen!({
        path: "../wit/sql",
        world: "wasi:sql/sql-host",
        async: true,
        with: {
            "wasi:sql/types/connection": super::SqlConn,
            "wasi:sql/types/statement": super::SqlStatement,
            "wasi:sql/types/error": super::SqlError,
        },
    });
}
use wasmtime::component::HasSelf;
use wasmtime_wasi::p2::IoView;
pub use bindings::wasi::sql::types::{DataType, Row};

use crate::runtime::ComponentCtx;

/// Host rep of the `error` resource.
pub struct SqlError(pub String);

/// Host rep of the `statement` resource.
pub struct SqlStatement {
    query: String,
    params: Vec<String>,
}

pub(crate) struct SqlxBackend {
    pool: sqlx::AnyPool,
    /// postgres needs `?` → `$N` rewriting; mysql/sqlite accept `?`.
    pg: bool,
}

/// A resolved, connected backend behind the `connection` resource.
pub(crate) enum Backend {
    Sqlx(SqlxBackend),
    ClickHouse(ClickHouse),
}

/// Host rep of the `connection` resource.
pub struct SqlConn(Arc<Backend>);

/// ClickHouse HTTP-endpoint client.
pub(crate) struct ClickHouse {
    url: String,
    user: Option<String>,
    password: Option<String>,
    http: reqwest::Client,
}

/// Everything a component invocation needs for wasi:sql.
pub struct SqlCtx {
    /// Revision dir — scopes the shared pool cache.
    pub dir: PathBuf,
    /// Data dir — for `local` database resolution.
    pub data_dir: PathBuf,
    /// App name — for `local` database resolution.
    pub app_name: String,
    /// Names granted by the manifest (`capabilities.sql`).
    pub allowed: Vec<String>,
    /// connection name → dsn, resolved from `$SECRET/` grants.
    pub dsns: HashMap<String, String>,
    /// Shared with `Runtime`: (revision-dir, name) → backend.
    pub(crate) pools: Arc<Mutex<HashMap<(PathBuf, String), Arc<Backend>>>>,
}
/// Resolve `capabilities.sql` grants into (allowed names, name → dsn).
/// Grants: `local` (reserved, app-scoped sqlite), `$SECRET/<name>` (DSN
/// from app secret `<name>` via `app_secret`), `$PLATFORM/<logical>`
/// (DSN from a platform-wide global secret, resolved via `platform_secret`
/// which applies the deploy-time binding logical → global name).
pub fn resolve_grants(
    grants: &[String],
    app: &str,
    mut app_secret: impl FnMut(&str) -> Option<String>,
    mut platform_secret: impl FnMut(&str) -> Option<String>,
) -> Result<(Vec<String>, HashMap<String, String>)> {
    let mut allowed = Vec::with_capacity(grants.len());
    let mut dsns = HashMap::new();
    for grant in grants {
        if grant == "local" {
            allowed.push(grant.clone());
            continue;
        }
        let (prefix, lookup, hint) = if let Some(key) = grant.strip_prefix("$SECRET/") {
            ("$SECRET/", &mut app_secret as &mut dyn FnMut(&str) -> Option<String>, format!("pushit secret set {app} {key} <dsn>"))
        } else if let Some(logical) = grant.strip_prefix("$PLATFORM/") {
            ("$PLATFORM/", &mut platform_secret as &mut dyn FnMut(&str) -> Option<String>, format!("bind platform_secrets: {{{logical}: <global-name>}} and set the global secret (pushit secret set --global <global-name> <dsn>)"))
        } else {
            bail!(
                "capabilities.sql: '{grant}' must be 'local', '$SECRET/<name>' or '$PLATFORM/<logical>'"
            );
        };
        let key = &grant[prefix.len()..];
        let dsn = lookup(key).with_context(|| {
            format!("capabilities.sql: no value for '{grant}' of app '{app}' ({hint})")
        })?;
        dsns.insert(key.to_string(), dsn);
        allowed.push(key.to_string());
    }
    Ok((allowed, dsns))
}

enum Driver {
    Sqlx,
    ClickHouse,
}

fn driver_for(dsn: &str) -> Result<Driver> {
    match dsn.split("://").next().unwrap_or("") {
        "postgres" | "postgresql" | "mysql" | "sqlite" => Ok(Driver::Sqlx),
        "clickhouse" | "clickhouse+http" | "ch" => Ok(Driver::ClickHouse),
        other => bail!("unsupported database scheme '{other}'"),
    }
}

async fn connect(dsn: &str) -> Result<Backend> {
    match driver_for(dsn)? {
        Driver::Sqlx => Ok(Backend::Sqlx(SqlxBackend {
            pg: dsn.starts_with("postgres"),
            pool: sqlx::any::AnyPoolOptions::new()
                .max_connections(4)
                .connect(dsn)
                .await
                .with_context(|| format!("connecting {dsn}"))?,
        })),
        Driver::ClickHouse => Ok(Backend::ClickHouse(clickhouse_connect(dsn)?)),
    }
}

/// `clickhouse://[user[:pass]@]host[:port][/db][?extra=params]` → HTTP client.
fn clickhouse_connect(dsn: &str) -> Result<ClickHouse> {
    let rest = dsn
        .split("://")
        .nth(1)
        .context("clickhouse dsn must be clickhouse://[user[:pass]@]host[:port][/db]")?;
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, p),
        None => (rest, ""),
    };
    let (auth, hostport) = match authority.rsplit_once('@') {
        Some((a, h)) => (Some(a), h),
        None => (None, authority),
    };
    let (user, password) = match auth {
        Some(a) => match a.split_once(':') {
            Some((u, p)) => (nonempty(u), nonempty(p)),
            None => (nonempty(a), None),
        },
        None => (None, None),
    };
    let hostport = if hostport.contains(':') {
        hostport.to_string()
    } else {
        format!("{hostport}:8123")
    };
    let (db, extra) = match path.split_once('?') {
        Some((d, q)) => (d, format!("&{q}")),
        None => (path, String::new()),
    };
    let mut url = format!("http://{hostport}/?default_format=JSONEachRow{extra}");
    if !db.is_empty() {
        url.push_str(&format!("&database={db}"));
    }
    Ok(ClickHouse {
        url,
        user,
        password,
        http: reqwest::Client::new(),
    })
}

fn nonempty(s: &str) -> Option<String> {
    if s.is_empty() { None } else { Some(s.to_string()) }
}

/// Replace `?` placeholders with escaped ClickHouse string literals.
/// `'` → `\'`, `\` → `\\`; every param lands as a string literal.
fn ch_substitute(query: &str, params: &[String]) -> Result<String> {
    let mut out = String::with_capacity(query.len() + 16);
    let mut it = params.iter();
    let mut chars = query.chars().peekable();
    let mut in_str = false;
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                // '' inside a string is an escaped quote, not a terminator
                if in_str && chars.peek() == Some(&'\'') {
                    chars.next();
                    out.push('\'');
                    out.push('\'');
                } else {
                    in_str = !in_str;
                    out.push(c);
                }
            }
            '?' if !in_str => {
                let p = it.next().context("more '?' placeholders than params")?;
                out.push('\'');
                for ch in p.chars() {
                    match ch {
                        '\\' => out.push_str("\\\\"),
                        '\'' => out.push_str("\\'"),
                        _ => out.push(ch),
                    }
                }
                out.push('\'');
            }
            _ => out.push(c),
        }
    }
    if it.next().is_some() {
        bail!("more params than '?' placeholders");
    }
    Ok(out)
}

/// Rewrite `?` placeholders to postgres `$N` (skips quoted literals).
fn pg_rewrite(query: &str) -> String {
    let mut out = String::with_capacity(query.len() + 8);
    let mut n = 0u32;
    let mut chars = query.chars().peekable();
    let mut in_str = false;
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                if in_str && chars.peek() == Some(&'\'') {
                    chars.next();
                    out.push('\'');
                    out.push('\'');
                } else {
                    in_str = !in_str;
                    out.push(c);
                }
            }
            '?' if !in_str => {
                n += 1;
                out.push_str(&format!("${n}"));
            }
            _ => out.push(c),
        }
    }
    out
}

async fn sqlx_query(b: &SqlxBackend, st: &SqlStatement) -> Result<Vec<Row>> {
    use sqlx::Row as _;
    let sql = if b.pg { pg_rewrite(&st.query) } else { st.query.clone() };
    let mut q = sqlx::query(&sql);
    for p in &st.params {
        q = q.bind(p.as_str());
    }
    let rows = q.fetch_all(&b.pool).await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        for (i, col) in r.columns().iter().enumerate() {
            out.push(Row {
                field_name: col.name().to_string(),
                value: any_value(r, i),
            });
        }
    }
    Ok(out)
}

async fn sqlx_exec(b: &SqlxBackend, st: &SqlStatement) -> Result<u32> {
    let sql = if b.pg { pg_rewrite(&st.query) } else { st.query.clone() };
    let mut q = sqlx::query(&sql);
    for p in &st.params {
        q = q.bind(p.as_str());
    }
    let n = q.execute(&b.pool).await?.rows_affected();
    Ok(u32::try_from(n).unwrap_or(u32::MAX))
}

fn any_value(row: &sqlx::any::AnyRow, i: usize) -> DataType {
    use sqlx::Row as _;
    use sqlx::TypeInfo as _;
    use sqlx::ValueRef as _;
    if let Ok(v) = row.try_get_raw(i) {
        if v.is_null() {
            return DataType::Null;
        }
    }
    let ty = row
        .columns()
        .get(i)
        .map(|c| c.type_info().name().to_ascii_uppercase())
        .unwrap_or_default();
    match ty.as_str() {
        "BOOL" | "BOOLEAN" => row
            .try_get::<bool, _>(i)
            .map(DataType::Boolean)
            .unwrap_or(DataType::Null),
        "INT2" | "SMALLINT" | "INT4" | "INTEGER" | "INT" | "SERIAL" => row
            .try_get::<i32, _>(i)
            .map(DataType::Int32)
            .or_else(|_| row.try_get::<i64, _>(i).map(DataType::Int64))
            .unwrap_or(DataType::Null),
        "INT8" | "BIGINT" => row
            .try_get::<i64, _>(i)
            .map(DataType::Int64)
            .unwrap_or(DataType::Null),
        "FLOAT4" | "REAL" | "FLOAT8" | "DOUBLE" | "DOUBLE PRECISION" | "NUMERIC"
        | "DECIMAL" | "FLOAT" => row
            .try_get::<f64, _>(i)
            .map(DataType::Double)
            .unwrap_or(DataType::Null),
        "DATE" => row
            .try_get::<String, _>(i)
            .map(DataType::Date)
            .unwrap_or(DataType::Null),
        "TIME" => row
            .try_get::<String, _>(i)
            .map(DataType::Time)
            .unwrap_or(DataType::Null),
        "TIMESTAMP" | "DATETIME" | "TIMESTAMPTZ" => row
            .try_get::<String, _>(i)
            .map(DataType::Timestamp)
            .unwrap_or(DataType::Null),
        "BLOB" | "BYTEA" | "BINARY" | "VARBINARY" => row
            .try_get::<Vec<u8>, _>(i)
            .map(DataType::Binary)
            .unwrap_or(DataType::Null),
        // TEXT/VARCHAR/JSON/UUID/unknown → str; undecodable → null.
        _ => row
            .try_get::<String, _>(i)
            .map(DataType::Str)
            .unwrap_or(DataType::Null),
    }
}

impl ClickHouse {
    async fn run(&self, sql: String) -> Result<String> {
        let mut req = self.http.post(&self.url).body(sql);
        if let Some(u) = &self.user {
            req = req.basic_auth(u, self.password.clone());
        }
        let resp = req.send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            bail!("clickhouse HTTP {status}: {}", text.trim());
        }
        Ok(text)
    }
}

fn ch_rows(text: &str) -> Result<Vec<Row>> {
    let mut out = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let obj: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(line).context("clickhouse JSONEachRow")?;
        for (k, v) in obj {
            out.push(Row {
                field_name: k,
                value: ch_value(v),
            });
        }
    }
    Ok(out)
}

fn ch_value(v: serde_json::Value) -> DataType {
    match v {
        serde_json::Value::Null => DataType::Null,
        serde_json::Value::Bool(b) => DataType::Boolean(b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i32::try_from(i).map(DataType::Int32).unwrap_or(DataType::Int64(i))
            } else if let Some(u) = n.as_u64() {
                u32::try_from(u).map(DataType::Uint32).unwrap_or(DataType::Uint64(u))
            } else {
                DataType::Double(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        serde_json::Value::String(s) => DataType::Str(s),
        other => DataType::Str(other.to_string()),
    }
}

impl ComponentCtx {
    fn sql_err(&mut self, msg: String) -> Resource<SqlError> {
        self.table()
            .push(SqlError(msg))
            .expect("ResourceTable::push failed")
    }
}

impl bindings::wasi::sql::types::Host for ComponentCtx {}
/// Resolve `name` against the manifest grant + secret-resolved DSNs.
/// `local` is a reserved name: auto-resolves to `$PUSHIT_DATA/appdata/<app>/local.db`
/// (app-scoped sqlite database, no secret needed).
fn authorize<'m>(
    name: &str,
    allowed: &'m [String],
    dsns: &'m HashMap<String, String>,
) -> Result<Option<&'m str>, String> {
    if !allowed.iter().any(|a| a == name) {
        return Err(format!("connection '{name}' not granted by capabilities.sql"));
    }
    if name == "local" {
        return Ok(None); // signal to use built-in local DSN
    }
    dsns.get(name)
        .map(|s| Some(s.as_str()))
        .ok_or_else(|| format!("connection '{name}' has no DSN (set it via pushit secret set)"))
}


impl bindings::wasi::sql::types::HostConnection for ComponentCtx {
    async fn open(&mut self, name: String) -> Result<Resource<SqlConn>, Resource<SqlError>> {
        let dsn_opt = match authorize(&name, &self.sql.allowed, &self.sql.dsns) {
            Ok(opt) => opt,
            Err(msg) => return Err(self.sql_err(msg)),
        };
        let dsn = match dsn_opt {
            Some(d) => d.to_string(),
            None => {
                // local: build DSN on the fly using data_dir
                let db_dir = self.sql.data_dir.join("appdata").join(&self.sql.app_name);
                if let Err(e) = std::fs::create_dir_all(&db_dir) {
                    return Err(self.sql_err(format!("failed to create appdata dir: {e}")));
                }
                let db_path = db_dir.join("local.db");
                // sqlx sqlite won't create the file; an empty file is a valid db
                std::fs::File::create(&db_path)
                    .map_err(|e| return self.sql_err(format!("failed to create local.db: {e}")))?;
                format!("sqlite://{}", db_path.display())
            }
        };
        let key = (self.sql.dir.clone(), name.clone());
        let cached = self.sql.pools.lock().get(&key).cloned();
        let backend = match cached {
            Some(b) => b,
            None => match connect(&dsn).await {
                Ok(b) => {
                    let b = Arc::new(b);
                    self.sql.pools.lock().insert(key, b.clone());
                    b
                }
                Err(e) => return Err(self.sql_err(format!("{e:#}"))),
            },
        };
        self.table()
            .push(SqlConn(backend))
            .map_err(|e| self.sql_err(format!("{e}")))
    }

    async fn drop(&mut self, rep: Resource<SqlConn>) -> Result<()> {
        self.table().delete(rep)?;
        Ok(())
    }
}

impl bindings::wasi::sql::types::HostStatement for ComponentCtx {
    async fn prepare(
        &mut self,
        query: String,
        params: Vec<String>,
    ) -> Result<Resource<SqlStatement>, Resource<SqlError>> {
        if query.trim().is_empty() {
            return Err(self.sql_err("empty query".into()));
        }
        self.table()
            .push(SqlStatement { query, params })
            .map_err(|e| self.sql_err(format!("{e}")))
    }

    async fn drop(&mut self, rep: Resource<SqlStatement>) -> Result<()> {
        self.table().delete(rep)?;
        Ok(())
    }
}

impl bindings::wasi::sql::types::HostError for ComponentCtx {
    async fn trace(&mut self, e: Resource<SqlError>) -> String {
        self.table()
            .get(&e)
            .map(|s: &SqlError| s.0.clone())
            .unwrap_or_else(|_| "<dropped error>".into())
    }

    async fn drop(&mut self, rep: Resource<SqlError>) -> Result<()> {
        self.table().delete(rep)?;
        Ok(())
    }
}

impl bindings::wasi::sql::readwrite::Host for ComponentCtx {
    async fn query(
        &mut self,
        c: Resource<SqlConn>,
        q: Resource<SqlStatement>,
    ) -> Result<Vec<Row>, Resource<SqlError>> {
        let (backend, st) = match self.borrow_stmt(&c, &q) {
            Ok(v) => v,
            Err(e) => return Err(e),
        };
        let res = match &*backend {
            Backend::Sqlx(b) => sqlx_query(b, &st).await,
            Backend::ClickHouse(ch) => match ch_substitute(&st.query, &st.params) {
                Ok(sql) => ch.run(sql).await.and_then(|t| ch_rows(&t)),
                Err(e) => Err(e),
            },
        };
        res.map_err(|e| self.sql_err(format!("{e:#}")))
    }

    async fn exec(
        &mut self,
        c: Resource<SqlConn>,
        q: Resource<SqlStatement>,
    ) -> Result<u32, Resource<SqlError>> {
        let (backend, st) = match self.borrow_stmt(&c, &q) {
            Ok(v) => v,
            Err(e) => return Err(e),
        };
        let res = match &*backend {
            Backend::Sqlx(b) => sqlx_exec(b, &st).await,
            Backend::ClickHouse(ch) => match ch_substitute(&st.query, &st.params) {
                Ok(sql) => ch.run(sql).await.map(|_| 0),
                Err(e) => Err(e),
            },
        };
        res.map_err(|e| self.sql_err(format!("{e:#}")))
    }
}

impl ComponentCtx {
    /// Clone the backend handle + statement out of the table so the async
    /// driver call doesn't hold a `&mut self` borrow.
    fn borrow_stmt(
        &mut self,
        c: &Resource<SqlConn>,
        q: &Resource<SqlStatement>,
    ) -> Result<(Arc<Backend>, SqlStatement), Resource<SqlError>> {
        let backend = self
            .table()
            .get(c)
            .map(|c: &SqlConn| c.0.clone())
            .map_err(|e| self.sql_err(format!("bad connection: {e}")))?;
        let st = self
            .table()
            .get(q)
            .map(|s: &SqlStatement| SqlStatement {
                query: s.query.clone(),
                params: s.params.clone(),
            })
            .map_err(|e| self.sql_err(format!("bad statement: {e}")))?;
        Ok((backend, st))
    }
}

/// Register wasi:sql imports on the linker.
pub fn add_to_linker(linker: &mut wasmtime::component::Linker<ComponentCtx>) -> Result<()> {
    bindings::wasi::sql::types::add_to_linker::<_, HasSelf<_>>(linker, |d| d)?;
    bindings::wasi::sql::readwrite::add_to_linker::<_, HasSelf<_>>(linker, |d| d)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_grants_mixed() {
        let grants = vec![
            "local".to_string(),
            "$SECRET/arcadia-db".to_string(),
            "$PLATFORM/shared".to_string(),
        ];
        let (allowed, dsns) = resolve_grants(
            &grants,
            "myapp",
            |k| (k == "arcadia-db").then(|| "postgres://u:p@h/db".to_string()),
            |l| (l == "shared").then(|| "postgres://u:p@shared/db".to_string()),
        )
        .unwrap();
        assert_eq!(allowed, ["local", "arcadia-db", "shared"]);
        assert_eq!(dsns["arcadia-db"], "postgres://u:p@h/db");
        assert_eq!(dsns["shared"], "postgres://u:p@shared/db");
    }

    #[test]
    fn resolve_grants_rejects_plain_names() {
        let err = resolve_grants(&["plain-db".to_string()], "myapp", |_| None, |_| None)
            .unwrap_err();
        assert!(err.to_string().contains("must be 'local', '$SECRET/<name>' or '$PLATFORM/<logical>'"));
    }

    #[test]
    fn resolve_grants_platform_unbound() {
        let err = resolve_grants(&["$PLATFORM/db".to_string()], "myapp", |_| None, |_| None)
            .unwrap_err();
        assert!(err.to_string().contains("no value for '$PLATFORM/db'"));
    }

    #[test]
    fn driver_scheme() {
        for d in [
            "postgres://u:p@h/db",
            "postgresql://u:p@h/db",
            "mysql://u:p@h/db",
            "sqlite:///tmp/x.db",
        ] {
            assert!(matches!(driver_for(d).unwrap(), Driver::Sqlx), "{d}");
        }
        for d in ["clickhouse://u:p@h:8123/db", "ch://h/db"] {
            assert!(matches!(driver_for(d).unwrap(), Driver::ClickHouse), "{d}");
        }
        assert!(driver_for("redis://h").is_err());
        assert!(driver_for("nonsense").is_err());
    }

    #[test]
    fn authorize_grant_and_dsn() {
        let allowed = vec!["sl".to_string()];
        let mut dsns = HashMap::new();
        dsns.insert("sl".to_string(), "sqlite:///tmp/x.db".to_string());
        assert_eq!(authorize("sl", &allowed, &dsns).unwrap().unwrap(), "sqlite:///tmp/x.db");
        // not granted
        assert!(authorize("pg", &allowed, &dsns).unwrap_err().contains("not granted"));
        // granted but no dsn configured
        let allowed2 = vec!["sl".to_string(), "pg".to_string()];
        assert!(authorize("pg", &allowed2, &dsns).unwrap_err().contains("has no DSN"));
        // granted but nothing configured at all
        assert!(authorize("sl", &allowed, &HashMap::new()).is_err());
        // local is reserved: granted via manifest, resolves without a DSN
        let allowed3 = vec!["local".to_string()];
        assert_eq!(authorize("local", &allowed3, &HashMap::new()).unwrap(), None);
    }

    #[test]
    fn ch_substitution_escapes() {
        let out = ch_substitute(
            "SELECT * FROM t WHERE a = ? AND b = 'lit?eral'",
            &["o'ha\\re".to_string()],
        )
        .unwrap();
        assert_eq!(
            out,
            "SELECT * FROM t WHERE a = 'o\\'ha\\\\re' AND b = 'lit?eral'"
        );
        // '' inside a literal is an escaped quote, not a terminator
        let out = ch_substitute("SELECT 'it''s ?' , ?", &["x".into()]).unwrap();
        assert_eq!(out, "SELECT 'it''s ?' , 'x'");
    }

    #[test]
    fn ch_substitute_count_mismatch() {
        assert!(ch_substitute("SELECT ?", &[]).is_err());
        assert!(ch_substitute("SELECT 1", &["x".into()]).is_err());
    }

    #[test]
    fn pg_rewrite_params() {
        assert_eq!(
            pg_rewrite("SELECT * FROM t WHERE a=? AND b='?' AND c=?"),
            "SELECT * FROM t WHERE a=$1 AND b='?' AND c=$2"
        );
    }

    #[test]
    fn clickhouse_dsn_parse() {
        let c = clickhouse_connect("clickhouse://u:p@ch:9000/analytics").unwrap();
        assert_eq!(
            c.url,
            "http://ch:9000/?default_format=JSONEachRow&database=analytics"
        );
        assert_eq!(c.user.as_deref(), Some("u"));
        assert_eq!(c.password.as_deref(), Some("p"));
        let c = clickhouse_connect("clickhouse://ch").unwrap();
        assert_eq!(c.url, "http://ch:8123/?default_format=JSONEachRow");
        assert!(c.user.is_none());
    }

}
