"""Exercise the built reconciler against mock vLLM and real LiteLLM/PostgreSQL."""

import json
import os
from http.client import HTTPException
from pathlib import Path
import subprocess
import time
from urllib.error import HTTPError
from urllib.request import Request, urlopen


ROOT = Path(__file__).resolve().parents[2]
LITELLM = "http://127.0.0.1:14000"
VLLM = "http://127.0.0.1:18000"
MODEL_A = "Qwen/Qwen3-32B"
MODEL_B = "Qwen/Qwen3-8B"


def request(base, path, key, body=None):
    data = None if body is None else json.dumps(body).encode()
    req = Request(base + path, data=data, headers={
        "Authorization": f"Bearer {key}", "Content-Type": "application/json",
    })
    with urlopen(req, timeout=10) as response:
        return json.load(response)


def litellm(path):
    return request(LITELLM, path, "sk-e2e-master-key")


def wait_for(description, check, timeout=180):
    deadline = time.monotonic() + timeout
    last_error = None
    while time.monotonic() < deadline:
        try:
            result = check()
            if result:
                return result
        except (OSError, HTTPException, ValueError, AssertionError) as error:
            last_error = error
        time.sleep(2)
    raise AssertionError(f"Timed out waiting for {description}: {last_error}")


def reconcile(dry_run=False):
    command = ["docker", "compose", "run", "--rm", "--no-deps", "-T", "reconciler",
               "--config", "/etc/reconciler/config.yaml", "--once"]
    if dry_run:
        command.append("--dry-run")
    env = {**os.environ, "COMPOSE_FILE": str(ROOT / "tests/e2e/compose.yaml"),
           "COMPOSE_PROJECT_NAME": "reconciler-e2e"}
    # A nonzero reconciler exit is a test failure; do not hide it with a retry.
    subprocess.run(command, cwd=ROOT, env=env, check=True, timeout=120)


def snapshot(expected):
    deployments = litellm("/model/info")["data"]
    managed = [d for d in deployments
               if d["model_info"].get("managed_by") == "vllm-reconciler"]
    assert len(managed) == len(expected), managed
    assert {d["model_name"] for d in managed} == set(expected), managed
    ids = {}
    for deployment in managed:
        name = deployment["model_name"]
        info = deployment["model_info"]
        assert info["source_id"] == "mock-gpu-01", info
        assert info["source_model_id"] == name, info
        assert info["external_key"] == f"mock-gpu-01::{name}", info
        assert deployment["litellm_params"]["model"] == f"hosted_vllm/{name}"
        assert deployment["litellm_params"]["api_base"] == "http://mock-vllm:8000/v1"
        assert info["id"], info
        ids[name] = info["id"]
    assert len(set(ids.values())) == len(ids), ids
    fixed = [d for d in deployments if d["model_name"] == "fixed-company-model"]
    assert len(fixed) == 1 and fixed[0]["model_info"].get("managed_by") != "vllm-reconciler"
    visible = [d["id"] for d in litellm("/v1/models")["data"]]
    assert sorted(visible) == sorted(["fixed-company-model", *expected]), visible
    return {**ids, "fixed-company-model": fixed[0]["model_info"]["id"]}


def set_models(models):
    request(VLLM, "/test/models", "e2e-vllm-key", {"models": models})


def main():
    wait_for("LiteLLM startup", lambda: litellm("/model/info"))
    wait_for("mock vLLM startup", lambda: request(VLLM, "/v1/models", "e2e-vllm-key"))
    # Confirm the authenticated discovery path is actually enforced.
    try:
        request(VLLM, "/v1/models", "wrong-key")
    except HTTPError as error:
        assert error.code == 401
    else:
        raise AssertionError("mock vLLM accepted an invalid API key")

    before = snapshot([])
    set_models([MODEL_A])
    reconcile(dry_run=True)
    assert snapshot([]) == before, "dry-run changed LiteLLM"
    reconcile()
    first = wait_for("first model registration", lambda: snapshot([MODEL_A]), timeout=30)
    assert first["fixed-company-model"] == before["fixed-company-model"]

    # Each compose run starts a fresh process: verify restart idempotency.
    reconcile()
    assert snapshot([MODEL_A]) == first, "restart changed deployment IDs"
    set_models([MODEL_A, MODEL_B])
    reconcile()
    second = wait_for("second model registration", lambda: snapshot([MODEL_A, MODEL_B]), timeout=30)
    assert all(second[name] == value for name, value in first.items())
    reconcile()
    assert snapshot([MODEL_A, MODEL_B]) == second, "duplicate deployment after restart"
    print("PASS: model discovery, dry-run, registration, /v1/models, restart idempotency, fixed model preservation", flush=True)


if __name__ == "__main__":
    main()
