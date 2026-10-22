#[allow(warnings)]
mod bindings;

use bindings::exports::wasi::http::incoming_handler::Guest;
use bindings::wasi::http::types::{
    Fields, IncomingRequest, OutgoingBody, OutgoingResponse, ResponseOutparam,
};
use bindings::wasi::blobstore::blobstore;
use bindings::wasi::blobstore::types::{IncomingValue, OutgoingValue};
use bindings::wasi::logging::logging::{self, Level};
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
            "/_fn/log" => {
                logging::log(Level::Info, "hello", "hello from wasi:logging");
                (200, "{\"ok\":true}".into())
            }
            "/_fn/blob" => match blob_roundtrip() {
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
    let conn = Connection::open("local").map_err(trace)?;
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

fn blob_roundtrip() -> Result<String, String> {
    use bindings::wasi::io::streams::StreamError;
    let c = blobstore::get_container("files")?;

    // Write 32MiB through an output-stream in chunks — proves streaming,
    // not one buffered blob. A few chunks go out before write-data binds
    // the key (spec order), the rest stream through the bound writer.
    const CHUNK: usize = 1 << 20;
    const TOTAL: u64 = 32 * CHUNK as u64;
    let out = OutgoingValue::new_outgoing_value();
    let body = out
        .outgoing_value_write_body()
        .map_err(|_| "write-body".to_string())?;
    let mut written = 0u64;
    for i in 0..4 {
        let chunk = vec![(i as u8).wrapping_add(0xAB); CHUNK];
        write_chunk(&body, &chunk)?;
        written += chunk.len() as u64;
    }
    c.write_data("big.bin", &out)?;
    while written < TOTAL {
        let chunk = vec![(written / CHUNK as u64) as u8; CHUNK];
        write_chunk(&body, &chunk)?;
        written += chunk.len() as u64;
    }
    drop(body);
    OutgoingValue::finish(out)?;

    // Read back via consume-async, iterating input-stream chunks.
    let inv = c.get_data("big.bin", 0, u64::MAX)?;
    let size = inv.size();
    let input = IncomingValue::incoming_value_consume_async(inv)?;
    let mut read = 0u64;
    loop {
        match input.blocking_read(CHUNK as u64) {
            Ok(b) => read += b.len() as u64,
            Err(StreamError::Closed) => break,
            Err(e) => return Err(format!("stream read: {e:?}")),
        }
    }
    if size != read {
        return Err(format!("size {size} != read {read}"));
    }

    // list objects, then clean up
    let names = c.list_objects()?;
    let (mut listed, mut done) = names.read_stream_object_names(64)?;
    while !done {
        let (more, d) = names.read_stream_object_names(64)?;
        listed.extend(more);
        done = d;
    }
    c.delete_object("big.bin")?;

    let listed_json = listed
        .iter()
        .map(|n| format!("{n:?}"))
        .collect::<Vec<_>>()
        .join(",");
    Ok(format!(
        "{{\"written\":{written},\"read\":{read},\"listed\":[{listed_json}]}}"
    ))
}

// blocking_write_and_flush accepts at most 4096 bytes per call.
fn write_chunk(
    s: &bindings::wasi::io::streams::OutputStream,
    mut b: &[u8],
) -> Result<(), String> {
    while !b.is_empty() {
        let n = b.len().min(4096);
        s.blocking_write_and_flush(&b[..n])
            .map_err(|e| format!("stream write: {e:?}"))?;
        b = &b[n..];
    }
    Ok(())
}
fn trace(e: Error) -> String {
    e.trace()
}

bindings::export!(Component with_types_in bindings);
