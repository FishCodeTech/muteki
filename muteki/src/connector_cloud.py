from __future__ import annotations

import asyncio
import json
import os
import shutil
from typing import Any
from urllib.parse import quote

from .connector_manifest import UpstreamPage, unavailable


async def fetch_repository(service: Any, task_id: str, provider: str, config: dict[str, Any]) -> dict[str, Any]:
    repository = str(config.get("repository") or "").strip().strip("/")
    if not repository:
        raise ValueError("repository is required")
    token = await service.resolve_secret(config, "token_ref", required=False)
    if provider == "github":
        base = str(config.get("base_url") or "https://api.github.com").rstrip("/")
        repo_path = quote(repository, safe="/")
        headers = {"Accept": "application/vnd.github+json", "X-GitHub-Api-Version": "2022-11-28"}
        if token:
            headers["Authorization"] = f"Bearer {token}"
        metadata_url = f"{base}/repos/{repo_path}"
    elif provider == "gitlab":
        base = str(config.get("base_url") or "https://gitlab.com").rstrip("/")
        repo_path = quote(repository, safe="")
        headers = {"Accept": "application/json"}
        if token:
            headers["PRIVATE-TOKEN"] = str(token)
        metadata_url = f"{base}/api/v4/projects/{repo_path}"
    else:
        raise ValueError(f"unsupported repository provider {provider}")
    metadata, raw_metadata, request = await service.fetch_json(task_id, metadata_url, headers=headers)
    if not isinstance(metadata, dict):
        raise ValueError("repository metadata is not an object")
    branch = str(config.get("ref") or metadata.get("default_branch") or "HEAD")
    if provider == "github":
        commit_url = f"{metadata_url}/commits/{quote(branch, safe='')}"
        commit_headers = headers
        stable_id = metadata.get("id")
        full_name = metadata.get("full_name") or repository
        web_url = metadata.get("html_url")
        visibility = metadata.get("visibility") or ("private" if metadata.get("private") else "public")
    else:
        commit_url = f"{metadata_url}/repository/commits/{quote(branch, safe='')}"
        commit_headers = headers
        stable_id = metadata.get("id")
        full_name = metadata.get("path_with_namespace") or repository
        web_url = metadata.get("web_url")
        visibility = metadata.get("visibility")
    commit, raw_commit, commit_request = await service.fetch_json(task_id, commit_url, headers=commit_headers)
    if not isinstance(commit, dict):
        raise ValueError("repository commit is not an object")
    commit_sha = commit.get("sha") or commit.get("id")
    source_refs = [
        service.artifact_for_source(raw_metadata, name=f"{provider}-repository.json", request=request, task_id=task_id),
        service.artifact_for_source(raw_commit, name=f"{provider}-commit.json", request=commit_request, task_id=task_id),
    ]
    item = {
        "input_key": f"{provider}:{stable_id or full_name}@{commit_sha or branch}",
        "mapped": {
            "type": "repository", "provider": provider,
            "repository_id": str(stable_id or full_name),
            "full_name": full_name, "default_branch": branch,
            "commit_sha": commit_sha, "web_url": web_url,
            "visibility": visibility, "permissions": metadata.get("permissions"),
            "active_operation_authorized": False,
        },
        "raw": {"metadata": metadata, "commit": commit},
    }
    return {"items": [item], "source_refs": source_refs, "repository": item["mapped"]}


async def aws_ec2_page(service: Any, config: dict[str, Any], next_token: str = "") -> UpstreamPage:
    executable = shutil.which("aws")
    if not executable:
        raise unavailable("AWS CLI is not installed; EC2 inventory cannot be collected")
    region = str(config.get("region") or "").strip()
    if not region:
        raise ValueError("AWS region is required")
    environment = await service.connector_environment(config)
    command = [
        executable, "ec2", "describe-instances", "--region", region,
        "--output", "json", "--max-items", str(int(config.get("page_size") or 100)),
    ]
    if config.get("profile"):
        command.extend(["--profile", str(config["profile"])])
    if next_token:
        command.extend(["--starting-token", next_token])
    process = await asyncio.create_subprocess_exec(
        *command, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
        env=environment,
    )
    try:
        stdout, stderr = await asyncio.wait_for(process.communicate(), timeout=float(config.get("timeout") or 45))
    except asyncio.TimeoutError:
        process.kill()
        await process.wait()
        raise TimeoutError("AWS CLI describe-instances timed out") from None
    if process.returncode:
        detail = stderr.decode("utf-8", errors="replace")
        raise RuntimeError(f"AWS CLI describe-instances failed ({process.returncode}): {detail}")
    document = json.loads(stdout)
    items: list[dict[str, Any]] = []
    for reservation in document.get("Reservations") or []:
        for instance in reservation.get("Instances") or []:
            instance_id = instance.get("InstanceId")
            items.append({
                "input_key": f"aws:ec2:{region}:{instance_id}",
                "mapped": {
                    "type": "cloud_resource", "provider": "aws", "service": "ec2",
                    "tenant": config.get("account_id"), "region": region,
                    "resource_id": instance_id, "state": (instance.get("State") or {}).get("Name"),
                    "public_read": False, "active_operation_authorized": False,
                },
                "raw": instance,
            })
    request = {"argv": command[:1] + command[1:], "credential_ref": config.get("credential_ref"), "region": region}
    request["argv"] = [part for part in request["argv"] if part]
    return UpstreamPage(
        items=items, raw=stdout, request=request,
        next_token=document.get("NextToken") or None,
        total=None, media_type="application/json",
    )


__all__ = ["aws_ec2_page", "fetch_repository"]
