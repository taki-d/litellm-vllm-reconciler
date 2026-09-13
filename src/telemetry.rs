use axum::{Router, extract::State, http::StatusCode, routing::get};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Default)]
pub struct Metrics {
    pub ready: bool,
    pub up: BTreeMap<String, bool>,
    pub discovered: BTreeMap<String, usize>,
    pub managed: usize,
    pub errors: u64,
    pub last_success: u64,
    pub changes: BTreeMap<String, u64>,
}
pub type Shared = Arc<Mutex<Metrics>>;
pub fn log(event: &str, fields: Value) {
    let mut value = fields.as_object().cloned().unwrap_or_default();
    value.insert("event".into(), json!(event));
    value.insert("timestamp".into(), json!(timestamp()));
    println!("{}", Value::Object(value));
}
pub fn timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub fn router(metrics: Shared) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok\n" }))
        .route("/readyz", get(|State(m): State<Shared>| async move {
            if m.lock().unwrap().ready { (StatusCode::OK, "ready\n") } else { (StatusCode::SERVICE_UNAVAILABLE, "not ready\n") }
        }))
        .route("/metrics", get(|State(m): State<Shared>| async move {
            let m = m.lock().unwrap();
            let mut text = format!("# TYPE reconciler_managed_deployments gauge\nreconciler_managed_deployments {}\n# TYPE reconciler_reconcile_errors_total counter\nreconciler_reconcile_errors_total {}\n# TYPE reconciler_last_success_timestamp gauge\nreconciler_last_success_timestamp {}\n", m.managed, m.errors, m.last_success);
            text.push_str("# TYPE reconciler_vllm_up gauge\n");
            for (id, up) in &m.up { text.push_str(&format!("reconciler_vllm_up{{source_id={id:?}}} {}\n", u8::from(*up))); }
            text.push_str("# TYPE reconciler_discovered_models gauge\n");
            for (id, count) in &m.discovered { text.push_str(&format!("reconciler_discovered_models{{source_id={id:?}}} {count}\n")); }
            text.push_str("# TYPE reconciler_changes_total counter\n");
            for (action, count) in &m.changes { text.push_str(&format!("reconciler_changes_total{{action={action:?}}} {count}\n")); }
            ([("content-type", "text/plain; version=0.0.4")], text)
        })).with_state(metrics)
}
