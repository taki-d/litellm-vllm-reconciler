use crate::{
    api::{Api, Deployment, desired},
    config::{Config, secret},
    telemetry::{Shared, log, timestamp},
};
use anyhow::{Result, bail};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

type Key = (String, String);
#[derive(Default)]
struct Failure {
    count: u64,
    since: Option<Instant>,
}
pub struct Reconciler {
    pub config: Config,
    api: Api,
    sources: BTreeMap<String, Api>,
    failures: BTreeMap<String, Failure>,
    missing: BTreeMap<String, Instant>,
    pub metrics: Shared,
}
impl Reconciler {
    pub fn new(config: Config, metrics: Shared) -> Result<Self> {
        let key = secret(&config.litellm.master_key_env)?;
        Self::with_master_key(config, metrics, key)
    }
    pub fn with_master_key(config: Config, metrics: Shared, key: String) -> Result<Self> {
        let api = Api::new(config.litellm.base_url.clone(), key, config.timeout())?;
        let mut sources = BTreeMap::new();
        for server in config.servers.iter().filter(|s| s.enabled) {
            let key = server
                .api_key_env
                .as_deref()
                .map(secret)
                .transpose()?
                .unwrap_or_default();
            sources.insert(
                server.id.clone(),
                Api::new(server.base_url.clone(), key, config.timeout())?,
            );
        }
        Ok(Self {
            config,
            api,
            sources,
            failures: BTreeMap::new(),
            missing: BTreeMap::new(),
            metrics,
        })
    }
    // &mut self and the single worker prohibit overlapping reconcile calls.
    pub async fn run_once(&mut self, now: Instant) -> Result<()> {
        let started = Instant::now();
        log(
            "reconcile_started",
            json!({"dry_run": self.config.reconcile.dry_run}),
        );
        let result = self.reconcile(now).await;
        let mut metrics = self.metrics.lock().unwrap();
        metrics.ready = result.is_ok();
        if result.is_ok() {
            metrics.last_success = timestamp();
        } else {
            metrics.errors += 1;
        }
        log(
            "reconcile_completed",
            json!({"success": result.is_ok(), "duration_ms": started.elapsed().as_millis()}),
        );
        result
    }
    async fn reconcile(&mut self, now: Instant) -> Result<()> {
        let mut wanted = BTreeMap::<Key, Deployment>::new();
        let mut healthy = BTreeSet::new();
        let mut discovery_failed = false;
        for server in self.config.servers.iter().filter(|s| s.enabled) {
            let started = Instant::now();
            let result = self.sources[&server.id].models().await;
            let failure = self.failures.entry(server.id.clone()).or_default();
            let mut metrics = self.metrics.lock().unwrap();
            metrics.up.insert(server.id.clone(), result.is_ok());
            match result {
                Ok(models) => {
                    *failure = Failure::default();
                    healthy.insert(server.id.clone());
                    metrics.discovered.insert(server.id.clone(), models.len());
                    log(
                        "vllm_discovery_succeeded",
                        json!({"source_id": server.id, "models": models.len(), "duration_ms": started.elapsed().as_millis()}),
                    );
                    for model in models {
                        wanted.insert((server.id.clone(), model.clone()), desired(server, &model));
                    }
                }
                Err(error) => {
                    failure.count = failure.count.saturating_add(1);
                    failure.since.get_or_insert(now);
                    metrics.discovered.insert(server.id.clone(), 0);
                    discovery_failed = true;
                    log(
                        "vllm_discovery_failed",
                        json!({"source_id": server.id, "consecutive_failures": failure.count, "error": error.to_string(), "duration_ms": started.elapsed().as_millis()}),
                    );
                }
            }
        }
        let current = self.api.current().await?;
        let mut managed = index(&current)?;
        self.metrics.lock().unwrap().managed = managed.len();
        // Reset timers as soon as a model reappears, even if a later mutation fails.
        self.missing.retain(|id, _| {
            managed
                .iter()
                .any(|(key, d)| d.id() == Some(id) && !wanted.contains_key(key))
        });
        let mut changed = false;
        for (key, target) in &wanted {
            let previous = managed.get(key);
            if previous.is_some_and(|d| d.matches(target)) {
                self.event("unchanged", target, 0);
                continue;
            }
            let mut target = target.clone();
            let action = if let Some(previous) = previous {
                target.model_info["id"] = json!(previous.id().unwrap());
                "updated"
            } else {
                // A deterministic ID must never take ownership of another deployment.
                if current.iter().any(|d| d.id() == target.id()) {
                    bail!("deployment ID ownership conflict");
                }
                "created"
            };
            if self.config.reconcile.dry_run {
                self.event(action, &target, 0);
                continue;
            }
            changed = true;
            let started = Instant::now();
            let result = if action == "created" {
                self.api.create(&target).await
            } else {
                self.api.update(&target).await
            };
            if result.is_ok() {
                self.event(action, &target, started.elapsed().as_millis());
            } else {
                log(
                    "deployment_apply_failed",
                    json!({"source_id": key.0, "model_name": key.1, "action": action}),
                );
            }
            // Even an error can mean the write committed. Read back before any deletion.
        }
        if changed {
            managed = index(&self.api.current().await?)?;
            self.metrics.lock().unwrap().managed = managed.len();
            if wanted
                .iter()
                .any(|(key, target)| !managed.get(key).is_some_and(|d| d.matches(target)))
            {
                bail!("desired deployments not confirmed; deletion deferred");
            }
        }
        let grace = Duration::from_secs(self.config.reconcile.deletion_grace_seconds);
        for (key, deployment) in &managed {
            if wanted.contains_key(key) {
                continue;
            }
            let id = deployment.id().unwrap();
            let eligible = if self.sources.contains_key(&key.0) && !healthy.contains(&key.0) {
                let failure = &self.failures[&key.0];
                // An outage starts its own full grace period, independent of old absence.
                failure.count >= self.config.reconcile.failure_threshold
                    && failure
                        .since
                        .is_some_and(|since| now.saturating_duration_since(since) >= grace)
            } else {
                let since = self.missing.entry(id.into()).or_insert(now);
                now.saturating_duration_since(*since) >= grace
            };
            if !eligible {
                continue;
            }
            let started = Instant::now();
            if !self.config.reconcile.dry_run {
                self.api.delete(id).await?;
                self.missing.remove(id);
                self.metrics.lock().unwrap().managed -= 1;
            }
            self.event("deleted", deployment, started.elapsed().as_millis());
        }
        if discovery_failed {
            bail!("one or more discoveries failed");
        }
        Ok(())
    }
    fn event(&self, action: &str, deployment: &Deployment, duration: u128) {
        log(
            &format!("deployment_{action}"),
            json!({"source_id": deployment.model_info["source_id"], "model_name": deployment.model_name,
            "action": action, "duration_ms": duration, "dry_run": self.config.reconcile.dry_run}),
        );
        if !self.config.reconcile.dry_run && action != "unchanged" {
            *self
                .metrics
                .lock()
                .unwrap()
                .changes
                .entry(action.into())
                .or_default() += 1;
        }
    }
}
fn index(current: &[Deployment]) -> Result<BTreeMap<Key, Deployment>> {
    let mut managed = BTreeMap::new();
    let mut ids = BTreeSet::new();
    for deployment in current {
        if let Some(key) = deployment.key() {
            let Some(id) = deployment.id() else {
                bail!("managed deployment missing ID");
            };
            if !ids.insert(id) || managed.insert(key, deployment.clone()).is_some() {
                bail!("ambiguous managed deployment; refusing mutations");
            }
        }
    }
    Ok(managed)
}
