"""Immutable, message-owned visual replies, independent of Agent transports.

The publish/inline-image design follows T3 Code's HtmlRender service (MIT,
nightly cfa4f765). Muteki only reads images inside the thread's visual workspace.
"""
from __future__ import annotations

import base64
from dataclasses import dataclass
from hashlib import sha256
import io
import json
import os
from pathlib import Path
import re
import tempfile
from typing import Any, Callable
from urllib.parse import unquote, urlsplit
import warnings

from bs4 import BeautifulSoup
from PIL import Image, UnidentifiedImageError

MAX_SOURCE_BYTES = 1_000_000
MAX_IMAGE_BYTES = 5_000_000
MAX_DOCUMENT_BYTES = 10_000_000
_FENCE = re.compile(r"^[ \t]*(`{3,}|~{3,})muteki-visualize[ \t]*\r?\n(.*?)\r?\n\1[ \t]*$", re.M | re.S)
_MARKER = re.compile(r"(?:visualize(\{[^\n]*?\})|^[ \t]*(?:muteki-visualize|visualize)[ \t]+(\{[^\n]*\})[ \t]*$)", re.M)
_IMAGE = re.compile(r'''(["'`])((?:/(?!/)|\./|\.\./|file:///)[^\r\n"'`]+?\.(?:png|jpe?g|gif|webp|avif|bmp|ico))\1|url\(\s*((?:/(?!/)|\./|\.\./)[^\s"'`()]+?\.(?:png|jpe?g|gif|webp|avif|bmp|ico))\s*\)''', re.I)


class VisualizationError(ValueError):
    def __init__(self, code: str, message: str):
        self.code = code
        super().__init__(message)


@dataclass(frozen=True)
class VisualReference:
    path: str
    html: str | None = None
    title: str = ""


def references(text: str) -> list[VisualReference]:
    result = []
    for match in _FENCE.finditer(text):
        html = match.group(2).strip()
        result.append(VisualReference("inline:" + sha256(html.encode()).hexdigest(), html))
    for match in _MARKER.finditer(text):
        try:
            value = json.loads(match.group(1) or match.group(2))
            if isinstance(value.get("path"), str):
                result.append(VisualReference(value["path"], title=str(value.get("title") or "")))
        except (ValueError, TypeError, AttributeError):
            continue  # Invalid markers remain ordinary message text.
    return result


def _read(path: Path, limit: int) -> bytes:
    with path.open("rb") as source:
        content = source.read(limit + 1)
    if len(content) > limit:
        raise VisualizationError("visualization.too_large", f"{path.name} 超出 {limit:,} 字节，未截断或发布")
    return content


def _inline_images(html: str, root: Path, base: Path) -> str:
    images: dict[str, str] = {}
    size = len(html.encode())

    def replace(match: re.Match) -> str:
        nonlocal size
        reference = match.group(2) or match.group(3)
        if reference not in images:
            value = urlsplit(reference)
            if value.netloc or value.query or value.fragment:
                raise VisualizationError("visualization.image_path", "本地图片必须使用文件路径")
            path = Path(unquote(value.path))
            target = (base / path).resolve(strict=True)
            if not target.is_relative_to(root.resolve()):
                raise VisualizationError("visualization.image_scope", "图片必须位于当前对话的可视化目录内")
            content = _read(target, MAX_IMAGE_BYTES)
            try:
                with warnings.catch_warnings():
                    warnings.simplefilter("error", Image.DecompressionBombWarning)
                    with Image.open(io.BytesIO(content)) as image:
                        mime = Image.MIME.get(image.format or "")
                        image.verify()
                if not mime or not mime.startswith("image/"):
                    raise ValueError("unsupported image")
            except (UnidentifiedImageError, ValueError, OSError, Image.DecompressionBombError, Image.DecompressionBombWarning) as exc:
                raise VisualizationError("visualization.invalid_image", f"{target.name} 不是可验证的图片") from exc
            images[reference] = f"data:{mime};base64," + base64.b64encode(content).decode("ascii")
        encoded = images[reference]
        size += len(encoded.encode()) - len(reference.encode())
        if size > MAX_DOCUMENT_BYTES:
            raise VisualizationError("visualization.too_large", "内嵌图片后图形超过 10 MB，未截断或发布")
        start, end = match.span(2 if match.group(2) else 3)
        return match.group(0)[:start - match.start()] + encoded + match.group(0)[end - match.start():]

    return _IMAGE.sub(replace, html)


def snapshot(root: Path, workspace: Path, message_id: str, reference: VisualReference,
             assets: Callable[[], dict[str, str]]) -> dict[str, Any]:
    """Publish once per message/reference, including a durable failure receipt.

    A reply's source files and plugin assets can later change or disappear;
    neither changes an already-published reply. Concurrent readers publish
    atomically without replacing the first snapshot.
    """
    identity = sha256((message_id + "\0" + reference.path).encode()).hexdigest()
    target = root / (identity + ".json")
    if target.is_file():
        return json.loads(_read(target, MAX_DOCUMENT_BYTES * 2).decode())
    try:
        base = workspace
        if reference.html is None:
            path = Path(reference.path).expanduser().resolve(strict=True)
            if not path.is_relative_to(workspace.resolve()) or path.suffix.lower() != ".html":
                raise VisualizationError("visualization.file_scope", "图形文件必须位于当前对话的可视化目录内")
            html = _read(path, MAX_SOURCE_BYTES).decode("utf-8")
            base = path.parent
        else:
            html = reference.html
        if len(html.encode()) > MAX_SOURCE_BYTES:
            raise VisualizationError("visualization.too_large", "图形源码超过 1 MB，未截断或发布")
        html = _inline_images(html, workspace, base)
        title_node = BeautifulSoup(html, "html.parser").title
        title = reference.title or (title_node.get_text() if title_node else "")
        document = {"html": html, "assets": assets(), "title": title or "交互图", "documentId": identity, "messageId": message_id}
        if len(json.dumps(document, ensure_ascii=False).encode()) > MAX_DOCUMENT_BYTES * 2:
            raise VisualizationError("visualization.too_large", "图形快照超过存储范围，未截断或发布")
    except (OSError, UnicodeError, VisualizationError) as exc:
        document = {"error": {"code": getattr(exc, "code", "visualization.source_unavailable"), "message": str(exc)}}
    root.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=".publish-", dir=root)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as output:
            json.dump(document, output, ensure_ascii=False)
        try:
            os.link(temporary, target)
        except FileExistsError:
            return json.loads(_read(target, MAX_DOCUMENT_BYTES * 2).decode())
    finally:
        Path(temporary).unlink(missing_ok=True)
    return document
