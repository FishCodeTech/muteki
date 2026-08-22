"""Project Muteki 统一命令行入口。"""

from __future__ import annotations

import argparse
import json
import os
import sys
from pathlib import Path
from typing import Any

from muteki.updater import UpdateError, UpdateManager, apply_managed_environment
from muteki.version import project_root, version_payload


def _print_payload(payload: dict[str, Any], *, as_json: bool = False) -> None:
    if as_json:
        print(json.dumps(payload, ensure_ascii=False, indent=2))
        return
    message = payload.get("message")
    if message:
        print(message)
    if payload.get("error"):
        print(f"错误：{payload['error']}", file=sys.stderr)


def _launch(mode: str, arguments: list[str]) -> int:
    metadata = apply_managed_environment()
    if metadata:
        root = UpdateManager().root / "current"
        if root.exists():
            root = root.resolve()
        else:
            raise UpdateError("托管安装缺少 current 版本链接")
    else:
        root = project_root()
    script = root / "run.sh"
    if not script.is_file():
        raise UpdateError(f"启动脚本不存在：{script}")
    os.chdir(root)
    os.execv(str(script), [str(script), mode, *arguments])
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="muteki", description="Project Muteki 本地运行与升级工具")
    subparsers = parser.add_subparsers(dest="command", required=True)
    for mode in ("web", "tui"):
        command = subparsers.add_parser(mode, help=f"启动 {mode.upper()}")
        command.add_argument("arguments", nargs=argparse.REMAINDER)

    version = subparsers.add_parser("version", help="显示当前版本和安装形态")
    version.add_argument("--json", action="store_true")

    status = subparsers.add_parser("status", help="显示升级状态")
    status.add_argument("--json", action="store_true")
    status.add_argument("--root", type=Path)

    upgrade = subparsers.add_parser("upgrade", help="检查或安装指定版本")
    upgrade.add_argument("target", nargs="?", help="目标版本，例如 v0.3.2；省略时使用最新稳定版")
    upgrade.add_argument("--check", action="store_true", help="只检查，不下载或切换")
    upgrade.add_argument("--force", action="store_true", help="重新安装相同版本")
    upgrade.add_argument("--json", action="store_true")
    upgrade.add_argument("--root", type=Path)
    upgrade.add_argument("--manifest-url")
    upgrade.add_argument("--compose", action="store_true", help="升级版本化 Docker Compose 部署")

    install = subparsers.add_parser("install", help="创建或刷新托管安装")
    install.add_argument("target", nargs="?")
    install.add_argument("--force", action="store_true")
    install.add_argument("--json", action="store_true")
    install.add_argument("--root", type=Path)
    install.add_argument("--manifest-url")

    rollback = subparsers.add_parser("rollback", help="切换回上一已安装版本")
    rollback.add_argument("--json", action="store_true")
    rollback.add_argument("--root", type=Path)
    rollback.add_argument("--compose", action="store_true", help="回滚版本化 Docker Compose 部署")
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        if args.command in {"web", "tui"}:
            return _launch(args.command, list(args.arguments))
        if args.command == "version":
            payload = version_payload()
            if args.json:
                print(json.dumps(payload, ensure_ascii=False, indent=2))
            else:
                print(f"Muteki {payload['version']} ({payload['install_kind']})")
                if payload.get("commit"):
                    print(f"Commit: {payload['commit']}")
                print(f"Root: {payload['root']}")
            return 0
        manager = UpdateManager(getattr(args, "root", None), getattr(args, "manifest_url", None))
        if args.command == "status":
            payload = manager.status()
            if args.json:
                print(json.dumps(payload, ensure_ascii=False, indent=2))
            else:
                print(f"当前版本：{payload['current_version']}")
                print(f"安装形态：{payload['install_kind']}")
                print(f"升级状态：{payload['status']}")
                if payload.get("message"):
                    print(payload["message"])
            return 0
        if args.command in {"upgrade", "install"}:
            if getattr(args, "check", False):
                payload = manager.check(args.target)
            else:
                payload = manager.compose_upgrade(args.target) if args.command == "upgrade" and args.compose else manager.upgrade(args.target, force=args.force)
            _print_payload(payload, as_json=args.json)
            if payload.get("launcher") and not args.json:
                print(f"命令入口：{payload['launcher']}")
            return 0
        if args.command == "rollback":
            payload = manager.compose_rollback() if args.compose else manager.rollback()
            _print_payload(payload, as_json=args.json)
            return 0
    except (UpdateError, OSError, ValueError) as exc:
        if getattr(args, "json", False):
            print(json.dumps({"ok": False, "error": str(exc)}, ensure_ascii=False))
        else:
            print(f"升级操作失败：{exc}", file=sys.stderr)
        return 1
    parser.error("未知命令")
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
