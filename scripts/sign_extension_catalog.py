"""为一个扩展 Catalog 条目生成 Ed25519 签名材料。"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
from pathlib import Path

from muteki.extensions.catalog import CatalogEntry, ExtensionCatalog
from muteki.extensions.trust import public_key_from_seed, sign_ed25519


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("catalog")
    parser.add_argument("extension_id")
    parser.add_argument("version")
    parser.add_argument(
        "--private-key-file", required=True,
        help="包含 32 字节 hex 或 base64 Ed25519 私钥种子的本地文件",
    )
    args = parser.parse_args(argv)
    entry: CatalogEntry = ExtensionCatalog(args.catalog).resolve(
        args.extension_id, args.version)
    unsigned = entry.model_copy(update={
        "signature": "",
        "signature_algorithm": "ed25519",
        "trust_status": "unsigned",
    })
    message = ExtensionCatalog._signed_payload(unsigned)
    seed = Path(args.private_key_file).read_text(encoding="utf-8").strip()
    print(json.dumps({
        "publisher": entry.publisher,
        "algorithm": "ed25519",
        "public_key": public_key_from_seed(seed),
        "signature": sign_ed25519(seed, message),
        "signed_payload_sha256": hashlib.sha256(message).hexdigest(),
    }, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
