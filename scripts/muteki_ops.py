"""调用运行中 Muteki 的只读诊断与显式维护 API。"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from urllib.error import HTTPError
from urllib.parse import quote
from urllib.request import Request, urlopen


def request(
    base_url: str,
    path: str,
    *,
    token: str = "",
    method: str = "GET",
    body: dict | None = None,
) -> tuple[bytes, str]:
    headers = {"Accept": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    data = None
    if body is not None:
        headers["Content-Type"] = "application/json"
        data = json.dumps(body).encode()
    req = Request(base_url.rstrip("/") + path, data=data, headers=headers,
                  method=method)
    try:
        with urlopen(req, timeout=30) as response:  # noqa: S310 本地运维 URL
            return response.read(), response.headers.get_content_type()
    except HTTPError as exc:
        detail = exc.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"HTTP {exc.code}: {detail}") from exc


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", default="http://127.0.0.1:8000")
    parser.add_argument("--token", default="")
    sub = parser.add_subparsers(dest="command", required=True)
    for name in ("overview", "outbox", "recovery-preview", "maintenance-preview"):
        sub.add_parser(name)
    receipt = sub.add_parser("receipt")
    receipt.add_argument("command_id")
    bundle = sub.add_parser("diagnostic-bundle")
    bundle.add_argument("--output", default="muteki-diagnostic.zip")
    maintenance = sub.add_parser("maintenance-apply")
    maintenance.add_argument("--candidate", action="append", default=[])
    maintenance.add_argument("--confirm", action="store_true")
    args = parser.parse_args(argv)

    path = {
        "overview": "/api/operations/overview",
        "outbox": "/api/operations/outbox",
        "recovery-preview": "/api/operations/recovery-preview",
        "maintenance-preview": "/api/operations/maintenance-preview",
        "diagnostic-bundle": "/api/operations/diagnostic-bundle",
    }.get(args.command, "")
    method = "GET"
    body = None
    if args.command == "receipt":
        path = f"/api/operations/receipts/{quote(args.command_id, safe='')}"
    elif args.command == "maintenance-apply":
        if not args.confirm:
            parser.error("maintenance-apply requires --confirm")
        path = "/api/operations/maintenance"
        method = "POST"
        body = {"confirm": True, "candidate_ids": args.candidate}

    raw, content_type = request(
        args.url, path, token=args.token, method=method, body=body)
    if args.command == "diagnostic-bundle":
        output = Path(args.output).resolve()
        output.write_bytes(raw)
        print(output)
        return 0
    if content_type == "application/json":
        print(json.dumps(json.loads(raw), ensure_ascii=False, indent=2))
    else:
        print(raw.decode("utf-8", errors="replace"))
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except RuntimeError as exc:
        print(str(exc), file=sys.stderr)
        sys.exit(1)
