"""C17: typed workspace file preview classification."""

from __future__ import annotations

import tempfile
import unittest
from pathlib import Path

from muteki.conversation.workspace_surfaces import (
    WorkspaceSurfaceError,
    ascii_filename_fallback,
    classify_preview_kind,
    content_disposition_header,
    is_unsafe_inline_media_type,
    read_workspace_file,
    safe_raw_content_headers,
)


class TypedWorkspaceFilePreviewTest(unittest.TestCase):
    def test_code_markdown_image_binary_and_missing(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "app.py").write_text("def hello():\n    return 1\n", encoding="utf-8")
            (root / "note.md").write_text("# Title\n\nbody\n", encoding="utf-8")
            (root / "pic.png").write_bytes(b"\x89PNG\r\n\x1a\n" + b"\x00" * 16)
            (root / "blob.bin").write_bytes(b"\x00\x01\x02\xff")
            (root / "page.html").write_text("<html><body>hi</body></html>\n", encoding="utf-8")

            code = read_workspace_file(str(root), "app.py", line=2)
            self.assertEqual(code["preview_kind"], "code")
            self.assertEqual(code["media_type"], "text/x-python")
            self.assertIn("def hello", code["content"])
            self.assertEqual(code["line"], 2)

            md = read_workspace_file(str(root), "note.md")
            self.assertEqual(md["preview_kind"], "markdown")
            self.assertTrue(md["content"].startswith("# Title"))

            image = read_workspace_file(str(root), "pic.png")
            self.assertEqual(image["preview_kind"], "image")
            self.assertEqual(image["media_type"], "image/png")
            self.assertIsNone(image["content"])

            binary = read_workspace_file(str(root), "blob.bin")
            self.assertEqual(binary["preview_kind"], "binary")
            self.assertIsNone(binary["content"])
            self.assertIn("下载", binary["message"] or "")

            html = read_workspace_file(str(root), "page.html")
            self.assertEqual(html["preview_kind"], "html")
            self.assertIn("沙箱", html["message"] or "")

            with self.assertRaises(WorkspaceSurfaceError) as ctx:
                read_workspace_file(str(root), "gone.txt")
            self.assertIn("不存在", str(ctx.exception))

    def test_too_large_reports_limit_without_content(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            big = root / "big.txt"
            big.write_bytes(b"a" * (512_000 + 10))
            data = read_workspace_file(str(root), "big.txt")
            self.assertEqual(data["preview_kind"], "too_large")
            self.assertIsNone(data["content"])
            self.assertEqual(data["max_preview_bytes"], 512_000)
            self.assertIn("500 KB", data["message"] or "")

    def test_classify_helpers(self) -> None:
        path = Path("x.pdf")
        self.assertEqual(classify_preview_kind(path, b"%PDF", 100), "pdf")
        self.assertEqual(classify_preview_kind(Path("x.bin"), b"\x00\x01", 2), "binary")

    def test_raw_html_forced_to_attachment_octet_stream(self) -> None:
        self.assertTrue(is_unsafe_inline_media_type("text/html"))
        self.assertTrue(is_unsafe_inline_media_type("image/svg+xml"))
        self.assertFalse(is_unsafe_inline_media_type("image/png"))
        self.assertFalse(is_unsafe_inline_media_type("application/pdf"))

        media, headers = safe_raw_content_headers(
            media_type="text/html",
            filename="evil.html",
            download=False,
        )
        self.assertEqual(media, "application/octet-stream")
        self.assertTrue(headers["Content-Disposition"].startswith("attachment;"))
        self.assertIn("evil.html", headers["Content-Disposition"])
        self.assertEqual(headers["X-Content-Type-Options"], "nosniff")

        png_media, png_headers = safe_raw_content_headers(
            media_type="image/png",
            filename="shot.png",
            download=False,
        )
        self.assertEqual(png_media, "image/png")
        self.assertTrue(png_headers["Content-Disposition"].startswith("inline;"))

    def test_unicode_content_disposition_is_latin1_safe(self) -> None:
        """#217: Chinese / emoji / spaces must not put raw Unicode in headers."""
        cases = [
            ("粘贴内容-1790406831382.txt", "inline"),
            ("报告 草稿.md", "attachment"),
            ("emoji-📎-notes.txt", "inline"),
            ("qa-ascii.txt", "inline"),
            ("spaced name.txt", "attachment"),
            ('weird"name\n.txt', "inline"),
        ]
        for name, disposition in cases:
            header = content_disposition_header(name, disposition=disposition)
            # Starlette encodes headers as latin-1 — this is the 500 root cause.
            header.encode("latin-1")
            self.assertTrue(
                header.startswith(f'{disposition}; filename="'),
                msg=header,
            )
            self.assertNotIn("\n", header)
            self.assertNotIn("\r", header)
            if any(ord(ch) > 0x7E for ch in name):
                self.assertIn("filename*=UTF-8''", header)
                # Raw CJK / emoji must not appear outside percent-encoding.
                for ch in name:
                    if ord(ch) > 0x7E:
                        self.assertNotIn(ch, header.split("filename*=", 1)[0])

        # Long-paste auto name → download keeps UTF-8 filename* payload.
        media, headers = safe_raw_content_headers(
            media_type="text/plain",
            filename="粘贴内容-1790406831382.txt",
            download=False,
        )
        self.assertEqual(media, "text/plain")
        cd = headers["Content-Disposition"]
        cd.encode("latin-1")
        self.assertTrue(cd.startswith("inline;"))
        self.assertIn("filename*=UTF-8''", cd)
        from urllib.parse import unquote
        star = cd.split("filename*=UTF-8''", 1)[1]
        self.assertEqual(unquote(star), "粘贴内容-1790406831382.txt")

        # User upload download branch + unsafe MIME still force attachment.
        media2, headers2 = safe_raw_content_headers(
            media_type="text/html",
            filename="说明文档.html",
            download=False,
        )
        self.assertEqual(media2, "application/octet-stream")
        self.assertTrue(headers2["Content-Disposition"].startswith("attachment;"))
        headers2["Content-Disposition"].encode("latin-1")
        self.assertIn("filename*=UTF-8''", headers2["Content-Disposition"])

        dl_media, dl_headers = safe_raw_content_headers(
            media_type="text/plain",
            filename="用户上传-报告.txt",
            download=True,
        )
        self.assertEqual(dl_media, "text/plain")
        self.assertTrue(dl_headers["Content-Disposition"].startswith("attachment;"))
        dl_headers["Content-Disposition"].encode("latin-1")

        # ASCII-only stays simple (no filename* required).
        ascii_cd = content_disposition_header("qa-ascii.txt", disposition="inline")
        self.assertEqual(ascii_cd, 'inline; filename="qa-ascii.txt"')
        self.assertEqual(ascii_filename_fallback("粘贴内容-1790406831382.txt").endswith(".txt"), True)

        # Optional: Starlette Response construction must not raise.
        try:
            from starlette.responses import Response
        except ImportError:
            return
        Response(content=b"ok", media_type=media, headers=headers)
        Response(content=b"ok", media_type=media2, headers=headers2)
        Response(content=b"ok", media_type=dl_media, headers=dl_headers)


if __name__ == "__main__":
    unittest.main()
