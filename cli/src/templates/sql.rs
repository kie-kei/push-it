#[allow(warnings)]
mod bindings;
use bindings::exports::wasi::http::incoming_handler::Guest;
use bindings::wasi::http::types::{Fields, IncomingRequest, OutgoingBody, OutgoingResponse, ResponseOutparam};
use bindings::wasi::sql::{readwrite, types::{Connection, Statement, DataType}};
use serde_json::{json, Value};
struct Component;
fn tables() -> Result<Value, String> {
    let connection = Connection::open("db").map_err(|e| e.trace())?;
    // For PostgreSQL/MySQL replace this ClickHouse system catalog query.
    let statement = Statement::prepare("SELECT database, name FROM system.tables LIMIT 50", &[]).map_err(|e| e.trace())?;
    let fields = readwrite::query(&connection, &statement).map_err(|e| e.trace())?;
    let mut rows = Vec::new();
    let mut row = serde_json::Map::new();
    for field in fields {
        if row.contains_key(&field.field_name) { rows.push(Value::Object(std::mem::take(&mut row))); }
        let value = match field.value {
            DataType::Int32(v) => json!(v), DataType::Int64(v) => json!(v),
            DataType::Uint32(v) => json!(v), DataType::Uint64(v) => json!(v),
            DataType::Float(v) | DataType::Double(v) => json!(v),
            DataType::Str(v) | DataType::Date(v) | DataType::Time(v) | DataType::Timestamp(v) => json!(v),
            DataType::Null => Value::Null,
            DataType::Boolean(v) => json!(v.to_string()),
            DataType::Binary(v) => json!(format!("{v:?}")),
        };
        row.insert(field.field_name, value);
    }
    if !row.is_empty() { rows.push(Value::Object(row)); }
    Ok(json!(rows))
}
impl Guest for Component {
    fn handle(request: IncomingRequest, out: ResponseOutparam) {
        let (status, value) = if request.path_with_query().unwrap_or_default().split('?').next() != Some("/_fn/tables") {
            (404, json!({"error": "not found"}))
        } else { match tables() { Ok(v) => (200, v), Err(e) => (500, json!({"error": e})) } };
        let headers = Fields::new();
        let _ = headers.set("content-type", &[b"application/json".to_vec()]);
        let response = OutgoingResponse::new(headers);
        let _ = response.set_status_code(status);
        let body = response.body().unwrap();
        ResponseOutparam::set(out, Ok(response));
        let stream = body.write().unwrap();
        let _ = stream.blocking_write_and_flush(value.to_string().as_bytes());
        drop(stream);
        let _ = OutgoingBody::finish(body, None);
    }
}
bindings::export!(Component with_types_in bindings);
