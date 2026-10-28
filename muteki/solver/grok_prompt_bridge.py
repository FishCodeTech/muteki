"""Send Grok a JSON content block through its native prompt-file input."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile


def main() -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False)
    parser.add_argument("--binary", required=True)
    args, native_args = parser.parse_known_args()
    prompt = sys.stdin.buffer.read().decode("utf-8")
    payload = json.dumps([{"type": "text", "text": prompt}], ensure_ascii=False).encode("utf-8")
    with tempfile.TemporaryDirectory(prefix="muteki-grok-prompt-") as directory:
        path = Path(directory) / "stdin.json"
        os.symlink("/dev/stdin", path)
        child = subprocess.Popen(
            [args.binary, *native_args, "--verbatim", "--prompt-file", str(path)],
            stdin=subprocess.PIPE,
        )
        child.communicate(payload)
        return child.returncode


if __name__ == "__main__":
    raise SystemExit(main())
