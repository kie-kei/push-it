#[allow(warnings)]
mod bindings;
use bindings::exports::wasi::http::incoming_handler::Guest;
use bindings::wasi::http::types::{Fields, IncomingRequest, OutgoingBody, OutgoingResponse, ResponseOutparam};
use bindings::wasi::blobstore::blobstore::get_container;
use serde_json::{json, Value};
struct Component;
fn objects() -> Result<Value, String> {
    let container = get_container("files")?;
    let stream = container.list_objects()?;
    let mut names = Vec::new();
    loop {
        let (batch, done) = stream.read_stream_object_names(100)?;
        names.extend(batch);
        if done { break; }
    }
    Ok(json!(names))
}
impl Guest for Component {
    fn handle(request: IncomingRequest, out: ResponseOutparam) {
        let (status, value) = if request.path_with_query().unwrap_or_default().split('?').next() != Some("/_fn/objects") {
            (404, json!({"error": "not found"}))
        } else { match objects() { Ok(v) => (200, v), Err(e) => (500, json!({"error": e})) } };
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
