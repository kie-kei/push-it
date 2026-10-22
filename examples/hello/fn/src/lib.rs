#[allow(warnings)]
mod bindings;

use bindings::exports::wasi::http::incoming_handler::Guest;
use bindings::wasi::http::types::{
    Fields, IncomingRequest, OutgoingBody, OutgoingResponse, ResponseOutparam,
};

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

bindings::export!(Component with_types_in bindings);
