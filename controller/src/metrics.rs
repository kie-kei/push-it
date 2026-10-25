use std::collections::HashMap;

/// Process-lifetime per-app traffic counters: (requests, errors).
/// In-memory only, reset on controller restart. A single lock per request is
/// fine for a single-VM internal platform; upgrade only on measured contention.
#[derive(Default)]
pub struct Metrics {
    counts: parking_lot::Mutex<HashMap<String, (u64, u64)>>,
}

impl Metrics {
    /// A request routed to the app (function or static) after auth.
    pub fn hit(&self, app: &str) {
        let mut counts = self.counts.lock();
        if let Some(count) = counts.get_mut(app) {
            count.0 += 1;
        } else {
            counts.insert(app.to_string(), (1, 0));
        }
    }

    /// A function invocation that ended in an HTTP >= 500 (trap, timeout, or
    /// the component's own 5xx). Static 404s are not counted.
    pub fn err(&self, app: &str) {
        if let Some(count) = self.counts.lock().get_mut(app) {
            count.1 += 1;
        }
    }

    pub fn get(&self, app: &str) -> (u64, u64) {
        self.counts.lock().get(app).copied().unwrap_or((0, 0))
    }
}
