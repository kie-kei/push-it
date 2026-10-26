use anyhow::{bail, Result};
pub struct Template {
    pub package: &'static str,
    pub manifest: &'static str,
    pub world: &'static str,
    pub code: &'static str,
    pub html: &'static str,
    pub deps: &'static [(&'static str, &'static str)],
}
pub fn get(name: &str) -> Result<Option<&'static Template>> {
    Ok(match name {
        "hello" => None,
        "clickhouse-dashboard" => Some(&SQL),
        "s3-browser" => Some(&BLOB),
        _ => bail!("unknown template {name:?}; available: hello, clickhouse-dashboard, s3-browser"),
    })
}
static SQL: Template = Template {
    package: "sql",
    manifest: r#"name: {name}
entry: fn/target/wasm32-wasip1/release/{crate_name}.wasm
capabilities: {sql: [db]}
sql_bindings: {db: 'secret:CLICKHOUSE_DSN'}
# Set with: pushit secret set {name} CLICKHOUSE_DSN=<dsn>
secrets: [CLICKHOUSE_DSN]
env: {}
build:
  fn: cd fn && cargo component build --release --target wasm32-wasip1
  ui_dist: ui
checks:
  - path: /_fn/tables
"#,
    world: "package pushit:{name};\nworld app {\n import wasi:sql/types@0.2.0-draft;\n import wasi:sql/readwrite@0.2.0-draft;\n export wasi:http/incoming-handler@0.2.0;\n}\n",
    deps: &[("sql/types.wit", include_str!("../../wit/sql/types.wit")), ("sql/readwrite.wit", include_str!("../../wit/sql/readwrite.wit"))],
    code: include_str!("templates/sql.rs"),
    html: r#"<!doctype html>
<meta charset="utf-8"><title>{name}</title><h1>Tables</h1><p id="error" role="alert"></p><table id="tables"></table>
<script>
fetch('/_fn/tables').then(async r => { const data = await r.json(); if (!r.ok) throw new Error(data.error); return data; }).then(rows => {
 const table = document.getElementById('tables');
 if (!rows.length) return;
 const header = table.insertRow();
 for (const key of Object.keys(rows[0])) { const th = document.createElement('th'); th.textContent = key; header.append(th); }
 for (const row of rows) { const tr = table.insertRow(); for (const value of Object.values(row)) tr.insertCell().textContent = value ?? ''; }
}).catch(e => document.getElementById('error').textContent = e.message);
</script>
"#,
};
static BLOB: Template = Template {
    package: "blobstore",
    manifest: r#"name: {name}
entry: fn/target/wasm32-wasip1/release/{crate_name}.wasm
capabilities: {blobstore: [files]}
# Bind files in $PUSHIT_DATA/blobstores.yaml; see docs/blobstore.md.
env: {}
secrets: []
build:
  fn: cd fn && cargo component build --release --target wasm32-wasip1
  ui_dist: ui
checks:
  - path: /_fn/objects
"#,
    world: "package pushit:{name};\nworld app {\n import wasi:blobstore/types@0.2.0-draft;\n import wasi:blobstore/container@0.2.0-draft;\n import wasi:blobstore/blobstore@0.2.0-draft;\n export wasi:http/incoming-handler@0.2.0;\n}\n",
    deps: &[("blobstore/types.wit", include_str!("../../examples/hello/fn/wit/deps/blobstore/types.wit")), ("blobstore/container.wit", include_str!("../../examples/hello/fn/wit/deps/blobstore/container.wit")), ("blobstore/blobstore.wit", include_str!("../../examples/hello/fn/wit/deps/blobstore/blobstore.wit"))],
    code: include_str!("templates/blob.rs"),
    html: r#"<!doctype html>
<meta charset="utf-8"><title>{name}</title><h1>Objects</h1><p id="error" role="alert"></p><ul id="objects"></ul>
<script>
fetch('/_fn/objects').then(async r => { const data = await r.json(); if (!r.ok) throw new Error(data.error); return data; }).then(names => {
 for (const name of names) { const li = document.createElement('li'); li.textContent = name; document.getElementById('objects').append(li); }
}).catch(e => document.getElementById('error').textContent = e.message);
</script>
"#,
};
