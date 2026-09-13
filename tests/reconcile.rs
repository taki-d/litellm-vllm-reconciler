use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, patch, post},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use vllm_reconciler::{
    api::{Api, Deployment, desired},
    config::{Config, LiteLlm, Reconcile, Server},
    reconciler::Reconciler,
    telemetry::{self, Shared},
};

#[derive(Default)]
struct Store {
    deployments: Vec<Deployment>,
    models: BTreeMap<String, Value>,
    status: BTreeMap<String, u16>,
    calls: Vec<String>,
    fail_create: bool,
    commit_then_fail: bool,
    fail_info: bool,
    hide_new: bool,
    create_delay_ms: u64,
}
type Db = Arc<Mutex<Store>>;
struct Fixture {
    db: Db,
    config: Config,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Fixture {
    async fn new() -> Self {
        let db = Db::default();
        let app = Router::new()
            .route("/{source}/models", get(discover))
            .route("/model/info", get(info))
            .route("/model/new", post(create))
            .route("/model/{id}/update", patch(update))
            .route("/model/delete", post(delete))
            .with_state(db.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let config = Config {
            litellm: LiteLlm {
                base_url: base.clone(),
                master_key_env: "TEST_MASTER_KEY".into(),
                request_timeout_seconds: 2,
            },
            reconcile: Reconcile {
                startup_delay_seconds: 0,
                deletion_grace_seconds: 60,
                ..Default::default()
            },
            servers: ["s1", "s2"]
                .into_iter()
                .map(|id| Server {
                    id: id.into(),
                    base_url: format!("{base}/{id}"),
                    api_key_env: None,
                    enabled: true,
                })
                .collect(),
            listen_address: "127.0.0.1:0".into(),
        };
        let f = Self { db, config, task };
        f.models("s1", &["A"]);
        f.models("s2", &["A"]);
        f
    }
    fn models(&self, source: &str, models: &[&str]) {
        self.db.lock().unwrap().models.insert(
            source.into(),
            json!({"data": models.iter().map(|id| json!({"id": id})).collect::<Vec<_>>()}),
        );
    }
    fn reconciler(&self) -> Reconciler {
        Reconciler::with_master_key(
            self.config.clone(),
            Shared::default(),
            "test-master-secret".into(),
        )
        .unwrap()
    }
    fn count(&self) -> usize {
        self.db.lock().unwrap().deployments.len()
    }
}
async fn discover(State(db): State<Db>, Path(source): Path<String>) -> (StatusCode, Json<Value>) {
    let mut db = db.lock().unwrap();
    db.calls.push(format!("discover:{source}"));
    (
        StatusCode::from_u16(*db.status.get(&source).unwrap_or(&200)).unwrap(),
        Json(db.models[&source].clone()),
    )
}
fn auth(headers: &HeaderMap) {
    assert_eq!(headers["authorization"], "Bearer test-master-secret");
}
async fn info(State(db): State<Db>, headers: HeaderMap) -> (StatusCode, Json<Value>) {
    auth(&headers);
    let mut db = db.lock().unwrap();
    db.calls.push("info".into());
    if db.fail_info {
        return (StatusCode::UNAUTHORIZED, Json(json!({})));
    }
    let mut deployments = db.deployments.clone();
    for d in &mut deployments {
        d.litellm_params["api_key"] = json!("********");
    }
    if db.hide_new {
        deployments.retain(|d| d.model_name != "B");
    }
    (StatusCode::OK, Json(json!({"data": deployments})))
}
async fn create(
    State(db): State<Db>,
    headers: HeaderMap,
    Json(d): Json<Deployment>,
) -> (StatusCode, Json<Value>) {
    auth(&headers);
    let delay = {
        let mut db = db.lock().unwrap();
        db.calls.push(format!("create:{}", d.model_name));
        db.create_delay_ms
    };
    tokio::time::sleep(Duration::from_millis(delay)).await;
    let mut db = db.lock().unwrap();
    if db.fail_create {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "test-master-secret"})),
        );
    }
    if db.deployments.iter().any(|old| old.id() == d.id()) {
        return (StatusCode::CONFLICT, Json(json!({})));
    }
    let id = d.id().unwrap().to_owned();
    db.deployments.push(d);
    if db.commit_then_fail {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({})));
    }
    (StatusCode::OK, Json(json!({"model_id": id})))
}
async fn update(
    State(db): State<Db>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(d): Json<Deployment>,
) -> Json<Value> {
    auth(&headers);
    assert_eq!(d.id(), Some(id.as_str()));
    let mut db = db.lock().unwrap();
    db.calls.push(format!("update:{}", d.model_name));
    *db.deployments
        .iter_mut()
        .find(|old| old.id() == Some(&id))
        .unwrap() = d;
    Json(json!({"model_id": id}))
}
async fn delete(State(db): State<Db>, headers: HeaderMap, Json(body): Json<Value>) -> Json<Value> {
    auth(&headers);
    let mut db = db.lock().unwrap();
    let d = db
        .deployments
        .iter()
        .find(|d| d.id() == body["id"].as_str())
        .unwrap();
    assert!(d.key().is_some(), "unmanaged deployment deleted");
    let name = d.model_name.clone();
    db.calls.push(format!("delete:{name}"));
    db.deployments.retain(|d| d.id() != body["id"].as_str());
    Json(json!({"message": "deleted"}))
}

#[tokio::test]
async fn register_balance_restart_and_preserve_unmanaged() {
    let f = Fixture::new().await;
    f.db.lock().unwrap().deployments.push(Deployment {
        model_name: "company-gpt".into(),
        litellm_params: json!({"model": "openai/company"}),
        model_info: json!({"id": "fixed"}),
    });
    let now = Instant::now();
    f.reconciler().run_once(now).await.unwrap();
    assert_eq!(f.count(), 3);
    let mut r = f.reconciler();
    r.run_once(now).await.unwrap();
    let db = f.db.lock().unwrap();
    assert_eq!(
        db.calls.iter().filter(|c| c.starts_with("create:")).count(),
        2
    );
    assert_eq!(
        db.deployments
            .iter()
            .filter(|d| d.model_name == "A")
            .count(),
        2
    );
    assert!(
        !db.calls
            .iter()
            .any(|c| c.starts_with("update:") || c.starts_with("delete:"))
    );
}

#[tokio::test]
async fn switch_adds_first_and_respects_grace() {
    let f = Fixture::new().await;
    let mut r = f.reconciler();
    let t = Instant::now();
    r.run_once(t).await.unwrap();
    f.models("s1", &["B"]);
    r.run_once(t + Duration::from_secs(1)).await.unwrap();
    assert_eq!(f.count(), 3);
    r.run_once(t + Duration::from_secs(60)).await.unwrap();
    assert_eq!(f.count(), 3);
    r.run_once(t + Duration::from_secs(61)).await.unwrap();
    assert_eq!(f.count(), 2);
    let db = f.db.lock().unwrap();
    assert!(
        db.calls.iter().position(|c| c == "create:B")
            < db.calls.iter().position(|c| c == "delete:A")
    );
    assert!(
        db.deployments
            .iter()
            .any(|d| d.model_name == "A" && d.model_info["source_id"] == "s2")
    );
}

#[tokio::test]
async fn outage_requires_threshold_and_grace_and_recovery_resets() {
    let f = Fixture::new().await;
    let mut r = f.reconciler();
    let t = Instant::now();
    r.run_once(t).await.unwrap();
    f.db.lock().unwrap().status.insert("s1".into(), 401);
    assert!(r.run_once(t).await.is_err());
    assert_eq!(f.count(), 2);
    assert!(r.run_once(t + Duration::from_secs(65)).await.is_err());
    assert_eq!(f.count(), 2);
    f.db.lock().unwrap().status.clear();
    r.run_once(t + Duration::from_secs(66)).await.unwrap();
    f.db.lock().unwrap().status.insert("s1".into(), 401);
    for sec in [70, 71, 72, 129] {
        assert!(r.run_once(t + Duration::from_secs(sec)).await.is_err());
        assert_eq!(f.count(), 2);
    }
    assert!(r.run_once(t + Duration::from_secs(130)).await.is_err());
    assert_eq!(f.count(), 1);
}

#[tokio::test]
async fn unsuccessful_or_unconfirmed_add_never_deletes_old() {
    let mut f = Fixture::new().await;
    f.config.reconcile.deletion_grace_seconds = 0;
    let mut r = f.reconciler();
    let t = Instant::now();
    r.run_once(t).await.unwrap();
    f.models("s1", &["B"]);
    f.db.lock().unwrap().fail_create = true;
    assert!(r.run_once(t).await.is_err());
    assert_eq!(f.count(), 2);
    {
        let mut db = f.db.lock().unwrap();
        db.fail_create = false;
        db.hide_new = true;
    }
    assert!(r.run_once(t).await.is_err());
    assert_eq!(f.count(), 3);
    f.db.lock().unwrap().hide_new = false;
    r.run_once(t).await.unwrap();
    assert_eq!(f.count(), 2);
}

#[tokio::test]
async fn ambiguous_write_does_not_duplicate() {
    let f = Fixture::new().await;
    f.db.lock().unwrap().commit_then_fail = true;
    f.reconciler().run_once(Instant::now()).await.unwrap();
    assert_eq!(f.count(), 2);
    f.reconciler().run_once(Instant::now()).await.unwrap();
    assert_eq!(f.count(), 2);
}

#[tokio::test]
async fn update_url_preserves_id_and_masked_keys_are_stable() {
    let mut f = Fixture::new().await;
    f.reconciler().run_once(Instant::now()).await.unwrap();
    let old = f.db.lock().unwrap().deployments[0].clone();
    // The same backend can be discovered through a different configured source URL.
    f.config.servers[0].base_url = f.config.servers[1].base_url.clone();
    let mut r = f.reconciler();
    r.run_once(Instant::now()).await.unwrap();
    r.run_once(Instant::now()).await.unwrap();
    let db = f.db.lock().unwrap();
    let updated = db.deployments.iter().find(|d| d.id() == old.id()).unwrap();
    assert_eq!(
        updated.litellm_params["api_base"],
        f.config.servers[0].base_url
    );
    assert_eq!(
        db.calls.iter().filter(|c| c.starts_with("update:")).count(),
        1
    );
}

#[tokio::test]
async fn removed_disabled_and_empty_sources_are_retired_after_grace() {
    let mut f = Fixture::new().await;
    f.reconciler().run_once(Instant::now()).await.unwrap();
    f.config.servers.remove(0);
    f.config.servers[0].enabled = false;
    let mut r = f.reconciler();
    let t = Instant::now();
    r.run_once(t).await.unwrap();
    assert_eq!(f.count(), 2);
    r.run_once(t + Duration::from_secs(60)).await.unwrap();
    assert_eq!(f.count(), 0);
    f.config.servers[0].enabled = true;
    f.reconciler().run_once(t).await.unwrap();
    assert_eq!(f.count(), 1);
    f.models("s2", &[]);
    let mut r = f.reconciler();
    r.run_once(t).await.unwrap();
    r.run_once(t + Duration::from_secs(60)).await.unwrap();
    assert_eq!(f.count(), 0);
}

#[tokio::test]
async fn dry_run_has_no_mutations() {
    let mut f = Fixture::new().await;
    f.reconciler().run_once(Instant::now()).await.unwrap();
    f.config.reconcile.dry_run = true;
    f.config.reconcile.deletion_grace_seconds = 0;
    f.models("s1", &["B"]);
    f.models("s2", &[]);
    f.db.lock().unwrap().calls.clear();
    f.reconciler().run_once(Instant::now()).await.unwrap();
    assert_eq!(f.count(), 2);
    assert!(
        f.db.lock()
            .unwrap()
            .calls
            .iter()
            .all(|c| c == "info" || c.starts_with("discover:"))
    );
}

#[tokio::test]
async fn malformed_discovery_is_failure_and_info_failure_blocks_writes() {
    let f = Fixture::new().await;
    let mut r = f.reconciler();
    let t = Instant::now();
    r.run_once(t).await.unwrap();
    f.db.lock()
        .unwrap()
        .models
        .insert("s1".into(), json!({"unexpected": []}));
    assert!(r.run_once(t).await.is_err());
    assert_eq!(f.count(), 2);
    f.models("s1", &["B"]);
    f.db.lock().unwrap().fail_info = true;
    assert!(r.run_once(t).await.is_err());
    assert_eq!(f.count(), 2);
    assert!(!r.metrics.lock().unwrap().ready);
}

#[tokio::test]
async fn duplicate_managed_entries_fail_closed() {
    let f = Fixture::new().await;
    let d = desired(&f.config.servers[0], "A");
    let mut other = d.clone();
    other.model_info["id"] = json!("duplicate");
    f.db.lock().unwrap().deployments.extend([d, other]);
    assert!(f.reconciler().run_once(Instant::now()).await.is_err());
    assert_eq!(f.count(), 2);
}

#[tokio::test]
async fn retries_only_server_errors_and_redacts_response_body() {
    let f = Fixture::new().await;
    let api = Api::new(
        f.config.servers[0].base_url.clone(),
        String::new(),
        Duration::from_secs(1),
    )
    .unwrap();
    for (status, expected) in [(500, 3), (401, 1), (429, 1), (302, 1)] {
        {
            let mut db = f.db.lock().unwrap();
            db.status.insert("s1".into(), status);
            db.calls.clear();
        }
        let error = api.models().await.unwrap_err().to_string();
        assert!(!error.contains("secret"));
        assert_eq!(f.db.lock().unwrap().calls.len(), expected);
    }
}

#[tokio::test]
async fn health_readiness_and_metrics() {
    let metrics = Shared::default();
    let app = telemetry::router(metrics.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .get(format!("{base}/healthz"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        client
            .get(format!("{base}/readyz"))
            .send()
            .await
            .unwrap()
            .status(),
        503
    );
    metrics.lock().unwrap().ready = true;
    assert_eq!(
        client
            .get(format!("{base}/readyz"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let body = client
        .get(format!("{base}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("reconciler_reconcile_errors_total 0"));
    task.abort();
}

#[test]
fn configuration_validation_and_identity_boundaries() {
    let text = include_str!("../examples/reconciler.yaml");
    let mut config: Config = serde_yaml::from_str(text).unwrap();
    config.validate().unwrap();
    config.servers.push(config.servers[0].clone());
    assert!(config.validate().is_err());
    config.servers.pop();
    config.servers[0].base_url = "http://user:secret@host/v1".into();
    let error = config.validate().unwrap_err().to_string();
    assert!(!error.contains("secret"));
    let a = Server {
        id: "one".into(),
        base_url: "http://localhost/v1".into(),
        api_key_env: None,
        enabled: true,
    };
    let d = desired(&a, "Qwen/Qwen3-32B");
    assert_eq!(d.id(), desired(&a, "Qwen/Qwen3-32B").id());
    let mut foreign = d.clone();
    foreign.model_info["managed_by"] = json!("human");
    assert!(foreign.key().is_none());
    foreign = d.clone();
    foreign.model_info["db_model"] = json!(false);
    assert!(foreign.key().is_none());
    foreign = d;
    foreign.model_info["external_key"] = json!("mismatch");
    assert!(foreign.key().is_none());
}

struct CliConfig {
    directory: std::path::PathBuf,
}
impl Drop for CliConfig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}
impl CliConfig {
    fn new(f: &Fixture) -> Self {
        let directory =
            std::env::temp_dir().join(format!("reconciler-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("key"), "test-master-secret\n").unwrap();
        let value = json!({
            "litellm": {"base_url": f.config.litellm.base_url, "master_key_env": "TEST_MASTER_KEY", "request_timeout_seconds": 2},
            "reconcile": {"startup_delay_seconds": 0, "interval_seconds": 60},
            "listen_address": "127.0.0.1:0",
            "servers": f.config.servers.iter().map(|s| json!({"id": s.id, "base_url": s.base_url})).collect::<Vec<_>>()
        });
        std::fs::write(
            directory.join("config.yaml"),
            serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
        Self { directory }
    }
    fn command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_vllm-reconciler"));
        command
            .arg("--config")
            .arg(self.directory.join("config.yaml"))
            .env_remove("TEST_MASTER_KEY")
            .env("TEST_MASTER_KEY_FILE", self.directory.join("key"))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        command
    }
}

#[tokio::test]
async fn cli_reads_secret_file_and_never_logs_master_key() {
    let f = Fixture::new().await;
    let files = CliConfig::new(&f);
    for fail in [false, true] {
        {
            let mut db = f.db.lock().unwrap();
            db.deployments.clear();
            db.fail_create = fail;
        }
        let mut command = files.command();
        command.arg("--once");
        let output = tokio::task::spawn_blocking(move || command.output().unwrap())
            .await
            .unwrap();
        assert_eq!(output.status.success(), !fail);
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(!stdout.contains("test-master-secret") && !stderr.contains("test-master-secret"));
        for line in stdout.lines() {
            assert!(serde_json::from_str::<Value>(line).is_ok());
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_finishes_in_flight_cycle() {
    let f = Fixture::new().await;
    f.db.lock().unwrap().create_delay_ms = 200;
    let files = CliConfig::new(&f);
    let mut child = files.command().spawn().unwrap();
    let started = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if f.db
                .lock()
                .unwrap()
                .calls
                .iter()
                .any(|c| c.starts_with("create:"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    if started.is_err() {
        let _ = child.kill();
        panic!("worker did not start");
    }
    assert!(
        std::process::Command::new("kill")
            .arg("-TERM")
            .arg(child.id().to_string())
            .status()
            .unwrap()
            .success()
    );
    let output = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap())
        .await
        .unwrap();
    assert!(output.status.success());
    assert_eq!(f.count(), 2);
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("shutdown_requested")
    );
}

#[tokio::test]
async fn reappearance_and_restart_reset_absence_grace() {
    let f = Fixture::new().await;
    let mut r = f.reconciler();
    let t = Instant::now();
    r.run_once(t).await.unwrap();
    f.models("s1", &[]);
    r.run_once(t).await.unwrap();
    f.models("s1", &["A"]);
    r.run_once(t + Duration::from_secs(30)).await.unwrap();
    f.models("s1", &[]);
    r.run_once(t + Duration::from_secs(40)).await.unwrap();
    r.run_once(t + Duration::from_secs(60)).await.unwrap();
    assert_eq!(f.count(), 2);
    let mut r = f.reconciler();
    r.run_once(t + Duration::from_secs(100)).await.unwrap();
    assert_eq!(f.count(), 2);
    r.run_once(t + Duration::from_secs(160)).await.unwrap();
    assert_eq!(f.count(), 1);
}
