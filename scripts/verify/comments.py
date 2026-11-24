import argparse
import json
import os
import re
import subprocess
import sys
from collections import Counter
from functools import cache
from pathlib import Path

from tree_sitter_language_pack import get_parser

LANGUAGES = {
    ".rs": "rust", ".py": "python", ".go": "go", ".mod": "gomod",
    ".ts": "typescript", ".tsx": "tsx", ".js": "javascript", ".jsx": "javascript",
    ".mjs": "javascript", ".cjs": "javascript", ".css": "css", ".html": "html",
    ".json": "json", ".jsonc": "json", ".svg": "xml", ".xml": "xml",
    ".mm": "objc", ".sh": "bash", ".toml": "toml", ".yml": "yaml", ".yaml": "yaml",
}
parser_for = cache(get_parser)


def language(path):
    if path.parts[0] in {"vendor", "reports"} or path.parts[:2] == ("docs", "issue-379-evidence"):
        return None
    if path.name in {".gitignore", ".dockerignore"} or path == Path("docker/worker/curated-packages.txt"):
        return "hash-lines"
    if path.name.split(".")[0] == "Dockerfile":
        return "dockerfile"
    if path.parts[:2] == (".github", "workflows") and path.suffix in {".yaml", ".yml"}:
        return "workflow"
    if path.parts[0] == ".githooks":
        return "bash"
    return LANGUAGES.get(path.suffix)


def comments(data, lang, offset=0):
    if lang == "hash-lines":
        for line in data.splitlines(keepends=True):
            if line.startswith(b"#"):
                yield offset, offset + len(line.rstrip(b"\r\n"))
            offset += len(line)
        return
    tree = parser_for("yaml" if lang == "workflow" else lang).parse(data)
    pending = [tree.root_node]
    while pending:
        node = pending.pop()
        if node.type in {"comment", "Comment", "line_comment", "block_comment"}:
            if lang == "dockerfile":
                line_start = data.rfind(b"\n", 0, node.start_byte) + 1
                if data[line_start:node.start_byte].strip():
                    continue
            start = offset + node.start_byte
            end = offset + node.end_byte
            yield start, end
        elif lang == "workflow" and node.type == "block_scalar" and node.text.startswith(b"|"):
            pair = node.parent.parent
            key = pair.child_by_field_name("key")
            if key and key.text == b"run":
                shell = "bash"
                for field in pair.parent.named_children:
                    field_key = field.child_by_field_name("key")
                    if field_key and field_key.text == b"shell":
                        shell = field.child_by_field_name("value").text.decode().split()[0]
                script_start = data.find(b"\n", node.start_byte, node.end_byte) + 1
                if shell in {"sh", "bash", "python"} and script_start > node.start_byte:
                    embedded = "python" if shell == "python" else "bash"
                    yield from comments(data[script_start:node.end_byte], embedded, offset + script_start)
        elif lang == "html" and node.type == "raw_text":
            embedded = {"script_element": "javascript", "style_element": "css"}.get(node.parent.type)
            if embedded:
                yield from comments(node.text, embedded, offset + node.start_byte)
        else:
            pending.extend(reversed(node.named_children))


def git(*args):
    return subprocess.check_output(["git", "--literal-pathspecs", *args])


def paths(*args):
    return [Path(os.fsdecode(p)) for p in git(*args).split(b"\0") if p]


def inventory(path, data):
    for start, end in comments(data, language(path)):
        raw = data[start:end].rstrip(b"\r\n")
        yield start, start + len(raw), raw


def build_directive(path, start, raw):
    if start == 0 and raw.startswith(b"#!"):
        return True
    if path.suffix == ".go":
        return re.fullmatch(rb"//go:build [A-Za-z0-9_.!&|() \t]+", raw) is not None
    if path.name.endswith(".d.ts"):
        return re.fullmatch(rb"""///\s*<reference\s+(?:types|lib|path)=("[^"<>]+"|'[^'<>]+')\s*/>""", raw) is not None
    return False


def main():
    cli = argparse.ArgumentParser()
    cli.add_argument("--inventory", action="store_true")
    cli.add_argument("--base", default=os.environ.get("MUTEKI_COMMENT_BASE", "HEAD"))
    cli.add_argument("--staged", action="store_true")
    args = cli.parse_args()
    os.chdir(os.fsdecode(git("rev-parse", "--show-toplevel")).strip())
    if args.inventory:
        for path in sorted(paths("ls-files", "-co", "--exclude-standard", "-z")):
            if language(path) and path.is_file() and not path.is_symlink():
                data = path.read_bytes()
                line = 1
                last = 0
                for start, end, raw in inventory(path, data):
                    line += data.count(b"\n", last, start)
                    last = start
                    print(json.dumps([str(path), line, start, end, raw.decode()], ensure_ascii=False))
        return 0
    base = args.base
    if base and set(base) == {"0"}:
        base = subprocess.check_output(["git", "hash-object", "-w", "-t", "tree", "--stdin"], input=b"").decode().strip()
    index = ["--cached"] if args.staged else []
    records = iter(git("diff", *index, "--find-renames", "--name-status", "--diff-filter=AMRT", "-z", base, "--").split(b"\0"))
    changed = {}
    for status in records:
        if not status:
            break
        path = Path(os.fsdecode(next(records)))
        if status.startswith(b"R"):
            destination = Path(os.fsdecode(next(records)))
            changed[destination] = path
        else:
            changed[path] = None if status == b"A" else path
    if not args.staged:
        for path in paths("ls-files", "--others", "--exclude-standard", "-z"):
            changed.setdefault(path, None)
    failed = False
    for path, original in sorted(changed.items()):
        if not language(path):
            continue
        if args.staged:
            stage = git("ls-files", "--stage", "--", str(path))
            if not stage.startswith(b"100"):
                continue
            data = git("show", f":{path.as_posix()}")
        else:
            if not path.is_file() or path.is_symlink():
                continue
            data = path.read_bytes()
        previous = Counter()
        if original is not None and language(original):
            old = git("show", f"{base}:{original.as_posix()}")
            previous.update(raw for _, _, raw in inventory(original, old))
        for start, _, raw in inventory(path, data):
            if previous[raw]:
                previous[raw] -= 1
            elif not build_directive(path, start, raw):
                line = data.count(b"\n", 0, start) + 1
                print(f"{path}:{line}: no-new-comments", file=sys.stderr)
                failed = True
    return int(failed)


if __name__ == "__main__":
    sys.exit(main())
