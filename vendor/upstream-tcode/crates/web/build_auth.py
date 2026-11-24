"""Embed browser authentication copy from the shared locale files."""
import json
from pathlib import Path

root = Path(__file__).resolve().parents[2]
translations = {}
for language in ("en", "zh-CN"):
    block = (root / "locales" / f"{language}.yml").read_text().split("\nweb_auth:\n", 1)[1]
    entries = {}
    for line in block.splitlines():
        if not line.startswith("  "):
            break
        key, value = line.strip().split(": ", 1)
        entries[key] = json.loads(value)
    translations[language] = entries
source = (root / "crates/web/static/auth.mjs").read_text()
dist = root / "crates/web/dist"
dist.mkdir(exist_ok=True)
(dist / "auth.mjs").write_text(source.replace("__AUTH_LOCALES__", json.dumps(translations, ensure_ascii=False)))
