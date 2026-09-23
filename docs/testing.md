# Testing

[Back to README](../README.md)

## Local checks

Run from the repository root:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
python3 -B -m unittest discover -s tests/e2e -p 'test_*.py'
```

Rust integration tests use HTTP mocks to cover registration, restart
idempotency, model replacement, deletion grace periods, outage recovery,
ownership boundaries, updates, dry-run behavior, retries, health endpoints,
secret handling, and graceful shutdown. Python unit tests cover E2E startup
polling, including connection resets and the readiness deadline.

These checks do not require Docker or a GPU. The HTTP integration tests need
permission to bind local ports.

## GitHub Actions

The [CI workflow](../.github/workflows/ci.yml) runs on pushes and pull requests.

| Job | Coverage |
| --- | --- |
| `test` | Rust formatting, Clippy, integration tests, and Python readiness tests. |
| `model-registration-e2e` | Model registration through the built reconciler container, real LiteLLM, and PostgreSQL. |

The E2E job starts an authenticated mock vLLM server, changes its model list
from empty to one model and then two, and checks:

- Dry-run mode leaves the LiteLLM catalog unchanged.
- Discovered models have the expected deployment metadata and API base URL.
- Registered models appear in LiteLLM's `/v1/models` response.
- New reconciler processes preserve deployment IDs and do not create duplicates.
- The fixed YAML model remains present with the same ID.

The job prints service logs and removes containers and database volumes even
after failure. All credentials in `tests/e2e` are public test fixtures for the
isolated stack. No GPU, external inference API, or GitHub secrets are needed.

## Run the E2E stack locally

Prerequisites: Docker with Compose, Python 3, and free local ports 14000 and
18000. Start each test with an empty test database. Run these commands from the
repository root:

```sh
export COMPOSE_FILE=tests/e2e/compose.yaml
export COMPOSE_PROJECT_NAME=reconciler-e2e
docker compose build reconciler
docker compose up -d db litellm mock-vllm
python3 tests/e2e/verify.py
```

Inspect the logs and clean up after the test, including when verification fails:

```sh
docker compose logs --no-color
docker compose down --volumes --remove-orphans
unset COMPOSE_FILE COMPOSE_PROJECT_NAME
```

The verification script waits for startup with a deadline. A nonzero reconciler
exit fails the test immediately; it is not hidden by retrying the entire cycle.

## Deployment acceptance checks

The mock implements discovery only. Inference and load balancing require real
vLLM servers and are outside the automated E2E test's scope.

Before production use, validate the following in your environment:

1. Serve the same model on two vLLM servers and confirm a single public model
   entry in LiteLLM and Open WebUI.
2. Send inference requests and verify that both backends receive traffic.
3. Stop one server and confirm requests can continue through the other.
4. Remove a model from every source and verify it disappears after the configured
   deletion conditions are met.
5. Switch a source to a new model and verify registration precedes retirement.
6. Confirm fixed models remain available and the Open WebUI key can access newly
   registered models.
