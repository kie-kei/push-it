#[allow(warnings)]
mod bindings;

use bindings::exports::wasi::http::incoming_handler::Guest;
use bindings::wasi::http::types::{
    Fields, IncomingRequest, OutgoingBody, OutgoingResponse, ResponseOutparam,
};
use bindings::wasi::sql::readwrite;
use bindings::wasi::sql::types::{Connection, DataType, Error, Statement};

struct Component;

impl Guest for Component {
    fn handle(request: IncomingRequest, out: ResponseOutparam) {
        let path = request.path_with_query().unwrap_or_default();
        let path = path.split('?').next().unwrap_or("/");

        let (status, body): (u16, String) = match path {
            "/_fn/hello" => {
                let user = request
                    .headers()
                    .get(&"x-user".to_string())
                    .first()
                    .and_then(|v| String::from_utf8(v.clone()).ok())
                    .map(|u| format!("\"{u}\""))
                    .unwrap_or_else(|| "null".into());
                (200, format!("{{\"msg\":\"hello\",\"user\":{user}}}"))
            }
            "/_fn/env" => {
                let v = std::env::var("GREETING").unwrap_or_default();
                (200, format!("{{\"GREETING\":\"{v}\"}}"))
            }
            "/_fn/sql" => match sql_roundtrip() {
                Ok(s) => (200, s),
                Err(e) => (500, format!("{{\"error\":{e:?}}}")),
            },
            _ => (404, "{\"error\":\"not found\"}".into()),
        };

        let headers = Fields::new();
        let _ = headers.set(&"content-type".to_string(), &[b"application/json".to_vec()]);
        let resp = OutgoingResponse::new(headers);
        let _ = resp.set_status_code(status);
        let body_out = resp.body().unwrap();
        ResponseOutparam::set(out, Ok(resp));
        let stream = body_out.write().unwrap();
        let _ = stream.blocking_write_and_flush(body.as_bytes());
        drop(stream);
        let _ = OutgoingBody::finish(body_out, None);
    }
}

fn sql_roundtrip() -> Result<String, String> {
    let conn = Connection::open("sl").map_err(trace)?;
    let ddl = Statement::prepare(
        "CREATE TABLE IF NOT EXISTS hello (id INTEGER PRIMARY KEY, msg TEXT)",
        &[],
    )
    .map_err(trace)?;
    readwrite::exec(&conn, &ddl).map_err(trace)?;
    let ins = Statement::prepare(
        "INSERT INTO hello (msg) VALUES (?)",
        &["hello from sql".to_string()],
    )
    .map_err(trace)?;
    readwrite::exec(&conn, &ins).map_err(trace)?;
    let sel = Statement::prepare("SELECT id, msg FROM hello ORDER BY id", &[]).map_err(trace)?;
    let rows = readwrite::query(&conn, &sel).map_err(trace)?;
    // rows arrive flattened: one record per column
    let mut fields = Vec::new();
    for r in &rows {
        let v = match &r.value {
            DataType::Null => "null".to_string(),
            DataType::Boolean(b) => b.to_string(),
            DataType::Int32(i) => i.to_string(),
            DataType::Int64(i) => i.to_string(),
            DataType::Uint32(u) => u.to_string(),
            DataType::Uint64(u) => u.to_string(),
            DataType::Float(f) | DataType::Double(f) => f.to_string(),
            DataType::Str(s) | DataType::Date(s) | DataType::Time(s)
            | DataType::Timestamp(s) => format!("{s:?}"),
            DataType::Binary(b) => format!("{b:?}"),
        };
        fields.push(format!("{:?}:{v}", r.field_name));
    }
    Ok(format!("{{\"rows\":[{}]}}", fields.join(",")))
}

fn trace(e: Error) -> String {
    e.trace()
}

bindings::export!(Component with_types_in bindings);
