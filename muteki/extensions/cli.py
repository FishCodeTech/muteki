"""扩展管理 CLI（EXT-01 的 CLI 后端；Web 前端由 EXT-02 负责）。

所有状态修改经 MutekiCommandAPI dispatch，与 Web 路由同一语义：

```bash
python -m muteki.extensions.cli --root state/control list
python -m muteki.extensions.cli install --kind local-dir --path /path/to/extension
python -m muteki.extensions.cli enable your.extension.id
python -m muteki.extensions.cli invoke your.extension.id ext.your.extension.id.greet --params '{"name":"muteki"}'
python -m muteki.extensions.cli health / logs / projection / disable / rollback / uninstall ...
```
"""

from __future__ import annotations

import argparse
import asyncio
import json
import sys
from pathlib import Path
from typing import Any

from muteki.extensions.handlers import register_extension_handlers
from muteki.extensions.permissions import EnvironmentSecretResolver
from muteki.extensions.registry import ExtensionService
from muteki.platform.command_api import MutekiCommandApiImpl
from muteki.platform.contracts.commands import (
    ActorRef,
    CommandEnvelope,
    QueryEnvelope,
)
from muteki.platform.contracts.receipts import ReceiptState
from muteki.platform.store import PlatformStore

OPERATOR = ActorRef(kind="operator", id="local-user")


def _build_stack(root: Path) -> tuple[PlatformStore, ExtensionService, Any]:
    """构造 store + service + command api 的最小栈（CLI 单进程使用）。"""
    store = PlatformStore(db_path=root / "platform.db")
    service = ExtensionService(
        store,
        install_root=root / "extensions" / "installed",
        state_root=root / "extensions" / "state",
        workspace_root=Path.cwd(),
        secret_resolver=EnvironmentSecretResolver(),
    )
    api = MutekiCommandApiImpl(store)
    register_extension_handlers(api, service)
    return store, service, api


async def _dispatch(api: Any, command_type: str, aggregate_id: str,
                    payload: dict[str, Any]) -> int:
    receipt = await api.dispatch(CommandEnvelope(
        command_type=command_type,
        aggregate_type="extension",
        aggregate_id=aggregate_id,
        actor=OPERATOR,
        payload=payload,
    ))
    print(json.dumps(receipt.model_dump(mode="json"), ensure_ascii=False,
                     indent=2))
    return 0 if receipt.state is ReceiptState.COMPLETED else 1


async def _query(api: Any, query_type: str, **params: Any) -> int:
    result = await api.query(QueryEnvelope(
        query_type=query_type,
        actor=OPERATOR,
        params={k: v for k, v in params.items() if v is not None},
    ))
    print(json.dumps(result.result, ensure_ascii=False, indent=2))
    return 0


def _source_from_args(args: argparse.Namespace) -> dict[str, Any]:
    source: dict[str, Any] = {"kind": args.kind}
    for key in ("path", "url", "ref", "sha256", "catalog_root", "version"):
        value = getattr(args, key, None)
        if value:
            source[key] = value
    if args.extension_id:
        source["extension_id"] = args.extension_id
    return source


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="muteki-extensions")
    parser.add_argument("--root", default="state/control",
                        help="控制根目录（platform.db 与扩展状态的父目录）")
    sub = parser.add_subparsers(dest="op", required=True)

    sub.add_parser("list")
    for name in ("get", "health", "logs", "projection", "disable",
                 "uninstall", "rollback"):
        p = sub.add_parser(name)
        p.add_argument("extension_id")
        if name == "projection":
            p.add_argument("--name", default="")
        if name == "rollback":
            p.add_argument("--version", default=None)
    p = sub.add_parser("install")
    p.add_argument("--kind", required=True,
                   choices=["local-dir", "archive", "git", "http", "catalog"])
    p.add_argument("--path", default="")
    p.add_argument("--url", default="")
    p.add_argument("--ref", default="")
    p.add_argument("--sha256", default="")
    p.add_argument("--catalog-root", dest="catalog_root", default="")
    p.add_argument("--extension-id", dest="extension_id", default="")
    p.add_argument("--version", default="")
    p = sub.add_parser("enable")
    p.add_argument("extension_id")
    p.add_argument("--version", default=None)
    p.add_argument("--config", default=None, help="JSON 字符串")
    p = sub.add_parser("upgrade")
    p.add_argument("extension_id")
    p.add_argument("--kind", required=True,
                   choices=["local-dir", "archive", "git", "http", "catalog"])
    p.add_argument("--path", default="")
    p.add_argument("--url", default="")
    p.add_argument("--ref", default="")
    p.add_argument("--sha256", default="")
    p.add_argument("--catalog-root", dest="catalog_root", default="")
    p.add_argument("--version", default="")
    p = sub.add_parser("invoke")
    p.add_argument("extension_id")
    p.add_argument("command_type")
    p.add_argument("--params", default="{}", help="JSON 字符串")

    args = parser.parse_args(argv)
    store, service, api = _build_stack(Path(args.root))

    async def _run() -> int:
        op = args.op
        if op == "list":
            return await _query(api, "extension.list")
        if op in ("get", "health", "logs", "projection"):
            params: dict[str, Any] = {"extension_id": args.extension_id}
            if op == "projection":
                params["name"] = args.name
            return await _query(api, f"extension.{op}", **params)
        if op == "install":
            return await _dispatch(api, "extension.install", "registry",
                                   {"source": _source_from_args(args)})
        if op == "enable":
            payload = {"extension_id": args.extension_id}
            if args.version:
                payload["version"] = args.version
            if args.config:
                payload["config"] = json.loads(args.config)
            return await _dispatch(
                api, "extension.enable", args.extension_id, payload)
        if op == "upgrade":
            payload = {"extension_id": args.extension_id,
                       "source": _source_from_args(args)}
            return await _dispatch(
                api, "extension.upgrade", args.extension_id, payload)
        if op == "rollback":
            payload = {"extension_id": args.extension_id}
            if args.version:
                payload["version"] = args.version
            return await _dispatch(
                api, "extension.rollback", args.extension_id, payload)
        if op in ("disable", "uninstall"):
            return await _dispatch(
                api, f"extension.{op}", args.extension_id,
                {"extension_id": args.extension_id})
        # invoke
        return await _dispatch(api, "extension.command", args.extension_id, {
            "extension_id": args.extension_id,
            "command_type": args.command_type,
            "params": json.loads(args.params),
        })

    try:
        return asyncio.run(_run())
    finally:
        store.close()


if __name__ == "__main__":
    sys.exit(main())
