use anyhow::{Result, bail};
use reqwest::{Client, Method};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

pub const OWNER: &str = "vllm-reconciler";
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Deployment {
    pub model_name: String,
    pub litellm_params: Value,
    pub model_info: Value,
}
impl Deployment {
    pub fn key(&self) -> Option<(String, String)> {
        let info = &self.model_info;
        if info["managed_by"] != OWNER || info["db_model"] == false {
            return None;
        }
        let source = info["source_id"].as_str()?;
        let model = info["source_model_id"].as_str()?;
        if source.is_empty()
            || model.is_empty()
            || info["external_key"] != format!("{source}::{model}")
        {
            return None;
        }
        Some((source.into(), model.into()))
    }
    pub fn id(&self) -> Option<&str> {
        self.model_info["id"].as_str().filter(|s| !s.is_empty())
    }
    pub fn matches(&self, desired: &Self) -> bool {
        self.model_name == desired.model_name
            && self.litellm_params["model"] == desired.litellm_params["model"]
            && self.litellm_params["api_base"] == desired.litellm_params["api_base"]
            // /model/info masks keys; compare the reference recorded in metadata.
            && self.model_info["reconciler_api_key_env"] == desired.model_info["reconciler_api_key_env"]
    }
}
pub fn desired(server: &crate::config::Server, model: &str) -> Deployment {
    let identity = serde_json::to_vec(&(OWNER, &server.id, model)).unwrap();
    let id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, &identity).to_string();
    let key_ref = server.api_key_env.as_deref().unwrap_or("");
    Deployment {
        model_name: model.into(),
        litellm_params: json!({"model": format!("hosted_vllm/{model}"), "api_base": server.base_url,
            "api_key": if key_ref.is_empty() { "EMPTY".into() } else { format!("os.environ/{key_ref}") }}),
        model_info: json!({"id": id, "managed_by": OWNER, "source_id": server.id, "source_model_id": model,
            "external_key": format!("{}::{model}", server.id), "reconciler_api_key_env": key_ref}),
    }
}
#[derive(Clone)]
pub struct Api {
    client: Client,
    base: String,
    key: String,
}
impl Api {
    pub fn new(base: String, key: String, timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .timeout(timeout)
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            base,
            key,
        })
    }
    pub async fn request(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        for attempt in 0..3u32 {
            let mut request = self
                .client
                .request(method.clone(), format!("{}{path}", self.base));
            if !self.key.is_empty() {
                request = request.bearer_auth(&self.key);
            }
            if let Some(body) = &body {
                request = request.json(body);
            }
            let response = request.send().await;
            let retry = match response {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        return response
                            .json()
                            .await
                            .map_err(|_| anyhow::anyhow!("invalid API JSON response"));
                    }
                    if !status.is_server_error() || attempt == 2 {
                        bail!("API HTTP {}", status.as_u16());
                    }
                    true
                }
                Err(error) => {
                    if !(error.is_connect() || error.is_timeout()) || attempt == 2 {
                        bail!("API transport failure");
                    }
                    true
                }
            };
            if retry {
                let jitter = u64::from(uuid::Uuid::new_v4().as_bytes()[0]);
                tokio::time::sleep(Duration::from_millis((200 << attempt) + jitter)).await;
            }
        }
        unreachable!()
    }
    pub async fn models(&self) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        struct Model {
            id: String,
        }
        #[derive(Deserialize)]
        struct List {
            data: Vec<Model>,
        }
        let value = self.request(Method::GET, "/models", None).await?;
        let list: List = serde_json::from_value(value)
            .map_err(|_| anyhow::anyhow!("invalid discovery response"))?;
        if list.data.iter().any(|m| m.id.trim().is_empty()) {
            bail!("empty model ID");
        }
        let mut ids: Vec<_> = list.data.into_iter().map(|m| m.id).collect();
        ids.sort();
        ids.dedup();
        Ok(ids)
    }
    pub async fn current(&self) -> Result<Vec<Deployment>> {
        #[derive(Deserialize)]
        struct List {
            data: Vec<Deployment>,
        }
        let value = self.request(Method::GET, "/model/info", None).await?;
        Ok(serde_json::from_value::<List>(value)
            .map_err(|_| anyhow::anyhow!("invalid model info response"))?
            .data)
    }
    pub async fn create(&self, deployment: &Deployment) -> Result<()> {
        self.request(
            Method::POST,
            "/model/new",
            Some(serde_json::to_value(deployment)?),
        )
        .await?;
        Ok(())
    }
    pub async fn update(&self, deployment: &Deployment) -> Result<()> {
        let mut url = reqwest::Url::parse(&format!("{}/model/", self.base))?;
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid URL"))?
            .pop_if_empty()
            .push(deployment.id().unwrap())
            .push("update");
        let path = url
            .as_str()
            .strip_prefix(&self.base)
            .ok_or_else(|| anyhow::anyhow!("invalid update path"))?;
        self.request(Method::PATCH, path, Some(serde_json::to_value(deployment)?))
            .await?;
        Ok(())
    }
    pub async fn delete(&self, id: &str) -> Result<()> {
        self.request(Method::POST, "/model/delete", Some(json!({"id": id})))
            .await?;
        Ok(())
    }
}
