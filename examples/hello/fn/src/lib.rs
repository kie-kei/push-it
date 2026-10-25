#[allow(warnings)]
mod bindings;

use bindings::exports::wasi::http::incoming_handler::Guest;
use bindings::wasi::blobstore::blobstore;
use bindings::wasi::blobstore::types::{IncomingValue, OutgoingValue};
use bindings::wasi::http::types::{
    Fields, IncomingRequest, OutgoingBody, OutgoingResponse, ResponseOutparam,
};
use bindings::wasi::keyvalue::store::{self, Bucket};
use bindings::wasi::logging::logging::{self, Level};

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
            // Diagnostic booleans only: never echo credential values. The local
            // authentication smoke uses the real component to verify the trust boundary.
            "/_fn/auth-check" => {
                let headers = request.headers();
                let ids = headers.get("x-user-id");
                let trusted_identity = ids == vec![b"local-owner".to_vec()];
                let cookies = headers.get("cookie");
                let cookie_text = cookies
                    .iter()
                    .map(|value| String::from_utf8_lossy(value))
                    .collect::<Vec<_>>()
                    .join(";");
                let platform_credentials =
                    !headers.get("authorization").is_empty() || cookie_text.contains("pushit_");
                let app_cookie = cookie_text.contains("app_preference=ok");
                (200, format!("{{\"trusted_identity\":{trusted_identity},\"platform_credentials\":{platform_credentials},\"app_cookie\":{app_cookie}}}"))
            }
            "/_fn/env" => {
                let v = std::env::var("GREETING").unwrap_or_default();
                (200, format!("{{\"GREETING\":\"{v}\"}}"))
            }
            "/_fn/kv" => match kv_roundtrip() {
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

fn kv_roundtrip() -> Result<String, String> {
    let kv = |e: bindings::wasi::keyvalue::store::Error| format!("{e:?}");
    let bucket: Bucket = store::open("local").map_err(kv)?;
    bucket.set("greeting", b"hello from kv").map_err(kv)?;
    let value = bucket.get("greeting").map_err(kv)?.unwrap_or_default();
    let keys = bucket.list_keys(None).map_err(kv)?.keys;
    bucket.delete("greeting").map_err(kv)?;
    let gone = bucket.exists("greeting").map_err(kv)?;
    Ok(format!(
        "{{\"value\":{:?},\"keys\":{:?},\"deleted\":{}}}",
        String::from_utf8_lossy(&value),
        keys,
        !gone
    ))
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
fn write_chunk(s: &bindings::wasi::io::streams::OutputStream, mut b: &[u8]) -> Result<(), String> {
    while !b.is_empty() {
        let n = b.len().min(4096);
        s.blocking_write_and_flush(&b[..n])
            .map_err(|e| format!("stream write: {e:?}"))?;
        b = &b[n..];
    }
    Ok(())
}

bindings::export!(Component with_types_in bindings);
