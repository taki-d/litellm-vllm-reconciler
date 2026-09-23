# Operations

[Back to README](../README.md)

## Ownership and identity

The reconciler only manages deployments whose metadata satisfies all of these
conditions:

- `model_info.managed_by` is `vllm-reconciler`.
- `source_id` and `source_model_id` are nonempty.
- `external_key` matches `<source_id>::<source_model_id>`.
- `db_model` is not explicitly `false`.

Deployments with incomplete ownership metadata are left untouched. If managed
deployments have ambiguous IDs or duplicate logical keys, the cycle stops before
making changes.

The logical key is `(source_id, source_model_id)`. New deployments receive a
deterministic UUID v5 in `model_info.id`, so a retry after a lost response uses
the same database ID. Existing deployments retain their actual IDs.

## Reconciliation lifecycle

Each cycle discovers models, reads LiteLLM's current catalog, applies additions
and updates, and then evaluates deletions. After attempting additions or updates,
the reconciler reads `GET /model/info` again to confirm the desired deployments.
If confirmation fails, all deletions in that cycle are deferred.

| Source state | Deletion behavior |
| --- | --- |
| Successful discovery no longer includes a model | Delete after the model's absence has lasted for the grace period. |
| Server is removed from configuration or disabled | Retire its owned deployments after the grace period. |
| Discovery fails | Require both the consecutive failure threshold and the outage grace period. |
| Discovery recovers | Reset the source's failure count and outage timer. |

Malformed JSON and invalid discovery responses count as failures. A valid empty
model list is a successful discovery with no desired deployments.

Grace timers and failure counts are held in memory and reset on restart. This
can postpone deletion but does not accelerate it. The deployment catalog is
reloaded from LiteLLM after each restart.

## Scheduling, retries, and shutdown

Run **one reconciler instance** per managed catalog. A single worker prevents
overlapping cycles within the process; multiple replicas are not supported.

The next cycle starts `interval_seconds` after the previous one finishes.
Discovery and writes run sequentially. Each HTTP request has a configured
timeout. Server errors (5xx), connection failures, and request-send timeouts are
retried up to three attempts with exponential backoff and jitter. Client errors
(4xx, including 429) are not retried within the request; subsequent cycles
evaluate the source again. Invalid response bodies fail the current operation.

SIGTERM and Ctrl-C allow the active cycle to finish before the process exits.
For larger catalogs, allow enough shutdown time for all sequential requests.
A rough cycle budget is the number of HTTP operations multiplied by three
request timeouts plus backoff per operation. The Compose example allows five
minutes for shutdown; adjust it for your deployment count.

## LiteLLM compatibility

Both Compose stacks pin LiteLLM to
`ghcr.io/berriai/litellm:main-v1.81.12-stable.2`. The API contract was checked
against the [source for that tag](https://github.com/BerriAI/litellm/blob/v1.81.12-stable.2/litellm/proxy/management_endpoints/model_management_endpoints.py).

| Operation | Endpoint | Notes |
| --- | --- | --- |
| List and verify | `GET /model/info` | Read deployment metadata and masked credentials. |
| Create | `POST /model/new` | Supply a stable `model_info.id`. |
| Update | `PATCH /model/{id}/update` | Update parameters, model name, and metadata. |
| Delete | `POST /model/delete` | Send `{"id": "<deployment-id>"}`. |

The legacy `POST /model/update` in this version only persists `litellm_params`.
The PATCH endpoint also saves the model name and ownership metadata, while
preserving extra metadata fields. Removing authentication writes `EMPTY` rather
than null, so the old key is not retained by merge semantics.

Database-backed deployments and YAML-defined models can coexist. See LiteLLM's
[model management documentation](https://docs.litellm.ai/docs/proxy/model_management).
Validate the management API and metadata round trip before changing the pinned
version.

## Health and metrics

Continuous mode exposes these endpoints on `listen_address` (default:
`0.0.0.0:9090`). They are not started in `--once` mode.

| Endpoint | Meaning |
| --- | --- |
| `/healthz` | Returns 200 while the HTTP server is running. |
| `/readyz` | Returns 200 after a successful cycle; returns 503 before the first success or after a failed cycle. |
| `/metrics` | Exposes Prometheus metrics. |

Readiness reflects the most recent completed cycle; it does not make a live
LiteLLM request for each probe. Discovery failure on any enabled vLLM server
makes that cycle unsuccessful.

| Metric | Meaning |
| --- | --- |
| `reconciler_vllm_up` | Discovery success per source: 1 for success, 0 for failure. |
| `reconciler_discovered_models` | Model count per source; 0 after discovery failure. |
| `reconciler_managed_deployments` | Last observed managed deployment count, adjusted for successful deletions. |
| `reconciler_reconcile_errors_total` | Failed reconciliation cycles. |
| `reconciler_last_success_timestamp` | Unix timestamp of the last successful cycle. |
| `reconciler_changes_total` | Successful write responses by action. |

Metrics reset on process restart. Dry-run cycles do not increment change
counters. A write that commits but returns an error can be confirmed by readback
without incrementing the change counter.

Logs contain one JSON object per line. Events include `reconcile_started`,
`vllm_discovery_succeeded`, `vllm_discovery_failed`, `deployment_created`,
`deployment_updated`, `deployment_deleted`, `deployment_unchanged`, and
`reconcile_completed`. Planned dry-run actions carry `dry_run: true`.

## Open WebUI integration

Point Open WebUI's OpenAI-compatible connection to the LiteLLM URL, such as
`http://litellm:4000/v1`, using a dedicated LiteLLM key for Open WebUI.

Ensure the key or team policy allows newly registered models. A static allowlist
can hide models even after registration succeeds. Open WebUI may also need to
refresh its cached model list.

Servers exposing the same model ID are registered under the same public model
name. LiteLLM handles inference routing with `simple-shuffle` in the supplied
configuration; see its [load balancing documentation](https://docs.litellm.ai/docs/proxy/load_balancing).
