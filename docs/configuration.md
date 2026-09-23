# Configuration

[Back to README](../README.md)

Start with [`examples/reconciler.yaml`](../examples/reconciler.yaml). Configuration
is loaded once at startup; restart the reconciler to apply changes.

## Settings

| Setting | Default | Description |
| --- | --- | --- |
| `litellm.base_url` | Required | LiteLLM proxy base URL, such as `http://litellm:4000`. |
| `litellm.master_key_env` | Required | Name of the environment variable containing the master key. |
| `litellm.request_timeout_seconds` | `10` | Timeout for each LiteLLM or vLLM HTTP request; valid range: 1–300 seconds. |
| `reconcile.interval_seconds` | `15` | Delay after a completed cycle before starting the next; must be positive. |
| `reconcile.failure_threshold` | `3` | Consecutive discovery failures required before outage-based deletion; must be positive. |
| `reconcile.deletion_grace_seconds` | `60` | Minimum absence or outage duration before deletion. |
| `reconcile.startup_delay_seconds` | `5` | Delay before the first cycle in continuous mode. |
| `reconcile.dry_run` | `false` | Log planned changes without writing to LiteLLM. |
| `listen_address` | `0.0.0.0:9090` | Socket address for health and metrics endpoints in continuous mode. |
| `servers` | Required | List of vLLM servers; an empty list retires owned deployments after the grace period. |
| `servers[].id` | Required | Stable, unique source identifier. |
| `servers[].base_url` | Required | vLLM API base URL, including `/v1`. |
| `servers[].api_key_env` | Unset | Name of the environment variable containing the vLLM API key. |
| `servers[].enabled` | `true` | Whether to discover models from this source. |

Source IDs accept ASCII letters, digits, hyphens, underscores, and dots. Keep an
ID unchanged when moving the same server to a new host or URL.

Base URLs must use HTTP or HTTPS and cannot contain credentials, query strings,
or fragments. Trailing slashes are removed. Unknown configuration fields and
duplicate source IDs are rejected at startup.

## Credentials and secret files

The reconciler reads a secret from its configured environment variable, or from
the file named by that variable with a `_FILE` suffix. For example:

```sh
export LITELLM_MASTER_KEY_FILE=/run/secrets/litellm_master_key
export VLLM_1_API_KEY_FILE=/run/secrets/vllm_1_api_key
```

If both forms are present, the direct environment variable takes precedence.
Trailing whitespace is removed from secret files. Empty secrets and secrets
containing line breaks are rejected.

For enabled servers, omitting `api_key_env` disables discovery authentication
and registers `api_key: EMPTY` with LiteLLM. When `api_key_env` is specified, its
secret must be available at startup. Disabled servers do not require secrets.

### LiteLLM must resolve the same vLLM keys

Deployment credentials are registered as references such as
`os.environ/VLLM_1_API_KEY`, not as literal secret values. Set the corresponding
environment variable in **both the reconciler and LiteLLM**.

The `_FILE` convention is implemented by the reconciler. When using secret
files, provide the resolved key through the ordinary environment variable on
LiteLLM separately. When rotating a key without changing its variable name,
update the environment of both processes.

The reconciler records the variable name in deployment metadata so that masked
keys returned by LiteLLM do not cause repeated updates.

## LiteLLM configuration

LiteLLM needs a database connection and `general_settings.store_model_in_db`
set to `true`. Keep static internal APIs in LiteLLM's YAML configuration and let
the reconciler manage vLLM deployments through the database.

See [`examples/litellm.yaml`](../examples/litellm.yaml) and
[`compose.yaml`](../compose.yaml) for the supplied setup. The Compose stack passes
the master key and vLLM key variables to both services.

## Logging and network access

The reconciler does not log Authorization headers, API response bodies, or
destination URLs. Model names and source IDs are logged, so do not put secrets
in those identifiers.

Keep the LiteLLM management API on a trusted network. The supplied Compose stack
binds published ports to localhost. Configure vLLM URLs that are reachable from
the reconciler and LiteLLM containers.
