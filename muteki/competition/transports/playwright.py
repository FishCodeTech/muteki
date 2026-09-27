"""授权浏览器会话传输（设计 4.4 / 8.3 第 2 顺位 / 13.3，COMP-02）。

只用于无稳定 REST 接口、且经用户明确授权的平台；REST 接口可用时，
浏览器不会成为默认传输（``select_transport_kind`` 保证）。

- Playwright 是**可选依赖**：未安装时所有浏览器操作抛
  ``TransportUnavailableError``（明确降级），进程不崩溃；storage
  目录管理等纯文件系统操作不依赖 Playwright。
- storage state 放在 ``control_root/competition/browser-profiles/
  <connection_id>/``：目录 0700、state 文件 0600，并写入 ``.gitignore``
  防入库；该目录不进入仓库、Run workspace、SharedGraph 或前端响应
  （核验文档 §PW：官方明示 storage state 含可冒用凭据，禁止入库）。
- storage state 覆盖 cookies/localStorage/IndexedDB/passkey，
  **不覆盖 sessionStorage**（官方无持久化 API）；依赖 sessionStorage
  存登录态的平台需要 ``add_init_script`` 回放，属平台级适配内容。
- 官方无自动续期：``probe_session`` 检测会话失效（重定向到登录页或
  401），失效时抛 ``PlatformAuthRequiredError``，连接转
  ``auth_required`` 由用户重新授权；``login_with_credentials`` 优先走
  ``APIRequestContext`` 的 API 登录（官方推荐，跳过 UI）。
"""

from __future__ import annotations

import json
import os
import stat
import hashlib
from datetime import datetime
from pathlib import Path
from typing import Any, Optional

from muteki.competition.platforms.base import (
    PlatformAuthRequiredError,
    PlatformTransportError,
    TransportUnavailableError,
)

_STORAGE_STATE_NAME = "state.json"
_STORAGE_META_NAME = "state.meta.json"
#: 目录内的防入库标记：browser-profiles 整体不进 git（核验 §PW 建议）。
_GITIGNORE_CONTENT = "*\n"

_INSTALL_HINT = (
    "playwright is not installed: run `pip install playwright "
    "&& playwright install chromium` to enable the browser transport"
)


def playwright_available() -> bool:
    """Playwright 是否可导入（不含浏览器二进制检查）。"""
    try:
        import playwright  # noqa: F401
    except ImportError:
        return False
    return True


class PlaywrightTransport:
    """单连接的授权浏览器会话。

    ``profile_root`` 为 ``control_root/competition/browser-profiles/``；
    每连接一个子目录隔离（核验 §PW：多账号各存各的 state 文件）。
    本类不持有也不记录登录密码：凭据由调用方在 ``login_with_credentials``
    调用期间短暂传入。
    """

    def __init__(
        self,
        profile_root: str | os.PathLike[str],
        connection_id: str,
        base_url: str,
        *,
        # 需登录的轻量端点，用于会话有效性探测；默认探测 base_url 本身。
        probe_path: str = "/",
        login_path: str = "",
    ) -> None:
        self._profile_root = Path(profile_root)
        self._connection_id = connection_id
        self._base_url = base_url.rstrip("/")
        self._probe_path = probe_path
        self._login_path = login_path

    # ------------------------------------------------------------------
    # storage state 目录（不依赖 Playwright）
    # ------------------------------------------------------------------

    @property
    def profile_dir(self) -> Path:
        return self._profile_root / self._connection_id

    @property
    def storage_state_path(self) -> Path:
        return self.profile_dir / _STORAGE_STATE_NAME

    @property
    def storage_metadata_path(self) -> Path:
        return self.profile_dir / _STORAGE_META_NAME

    def ensure_profile_dir(self) -> Path:
        """创建/校正 profile 目录：0700 + 防入库 .gitignore。"""
        for directory in (self._profile_root, self.profile_dir):
            try:
                current = directory.lstat()
            except FileNotFoundError:
                directory.mkdir(mode=0o700, parents=True, exist_ok=False)
                current = directory.lstat()
            if not stat.S_ISDIR(current.st_mode) or stat.S_ISLNK(current.st_mode):
                raise PlatformTransportError(
                    "browser profile path must be a real directory"
                )
            os.chmod(directory, 0o700)
        gitignore = self.profile_dir / ".gitignore"
        if not gitignore.exists():
            gitignore.write_text(_GITIGNORE_CONTENT, encoding="utf-8")
        return self.profile_dir

    def has_storage_state(self) -> bool:
        path = self.storage_state_path
        return path.is_file() and not path.is_symlink()

    def clear_storage_state(self) -> bool:
        """删除过期 state（核验 §PW：过期后需删除再重新登录）。"""
        path = self.storage_state_path
        removed = False
        for target in (path, self.storage_metadata_path):
            if target.exists():
                target.unlink()
                removed = True
        return removed

    def import_storage_state(
        self, value: str | bytes | dict[str, Any], *, expires_at: str = ""
    ) -> dict[str, Any]:
        """导入用户授权的 storage state；只返回摘要与有效期。"""
        if isinstance(value, bytes):
            raw = value.decode("utf-8")
            data = json.loads(raw)
        elif isinstance(value, str):
            raw = value
            data = json.loads(value)
        else:
            data = dict(value)
            raw = json.dumps(data, ensure_ascii=False, sort_keys=True)
        if not isinstance(data, dict):
            raise PlatformTransportError("browser storage state must be an object")
        cookies = data.get("cookies", [])
        origins = data.get("origins", [])
        if not isinstance(cookies, list) or not isinstance(origins, list):
            raise PlatformTransportError(
                "browser storage state requires cookies/origins arrays")
        normalized_expiry = ""
        if expires_at:
            try:
                parsed = datetime.fromisoformat(expires_at.replace("Z", "+00:00"))
            except ValueError as exc:
                raise PlatformTransportError(
                    "browser storage state expires_at is invalid") from exc
            normalized_expiry = parsed.isoformat()
        self.ensure_profile_dir()
        encoded = json.dumps(
            data, ensure_ascii=False, sort_keys=True).encode("utf-8")
        temporary = self.storage_state_path.with_suffix(".json.tmp")
        temporary.write_bytes(encoded)
        os.chmod(temporary, 0o600)
        temporary.replace(self.storage_state_path)
        metadata = {
            "sha256": hashlib.sha256(encoded).hexdigest(),
            "imported_at": datetime.now().astimezone().isoformat(),
            "expires_at": normalized_expiry,
            "cookies": len(cookies),
            "origins": len(origins),
        }
        meta_tmp = self.storage_metadata_path.with_suffix(".json.tmp")
        meta_tmp.write_text(
            json.dumps(metadata, ensure_ascii=False, sort_keys=True),
            encoding="utf-8")
        os.chmod(meta_tmp, 0o600)
        meta_tmp.replace(self.storage_metadata_path)
        return self.storage_state_status()

    def storage_state_status(self) -> dict[str, Any]:
        if not self.has_storage_state():
            return {"present": False, "expired": False}
        metadata: dict[str, Any] = {}
        try:
            metadata = json.loads(
                self.storage_metadata_path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            metadata = {}
        expires_at = str(metadata.get("expires_at") or "")
        expired = False
        if expires_at:
            try:
                expiry = datetime.fromisoformat(expires_at.replace("Z", "+00:00"))
                expired = datetime.now().astimezone() >= expiry
            except ValueError:
                expired = True
        return {
            "present": True,
            "expired": expired,
            "sha256": str(metadata.get("sha256") or ""),
            "imported_at": str(metadata.get("imported_at") or ""),
            "expires_at": expires_at,
            "cookies": int(metadata.get("cookies") or 0),
            "origins": int(metadata.get("origins") or 0),
        }

    def renew_storage_state(self, *, expires_at: str = "") -> dict[str, Any]:
        """只更新现有 storage state 的有效期，不把凭据内容送回 API。"""
        if not self.has_storage_state():
            raise PlatformAuthRequiredError(
                "browser storage state is missing; import it before renewal")
        try:
            metadata = json.loads(
                self.storage_metadata_path.read_text(encoding="utf-8"))
        except (OSError, ValueError) as exc:
            raise PlatformTransportError(
                "browser storage state metadata is unavailable") from exc
        normalized_expiry = ""
        if expires_at:
            try:
                normalized_expiry = datetime.fromisoformat(
                    expires_at.replace("Z", "+00:00")).isoformat()
            except ValueError as exc:
                raise PlatformTransportError(
                    "browser storage state expires_at is invalid") from exc
        metadata["expires_at"] = normalized_expiry
        metadata["renewed_at"] = datetime.now().astimezone().isoformat()
        temporary = self.storage_metadata_path.with_suffix(".json.tmp")
        temporary.write_text(
            json.dumps(metadata, ensure_ascii=False, sort_keys=True),
            encoding="utf-8",
        )
        os.chmod(temporary, 0o600)
        temporary.replace(self.storage_metadata_path)
        return self.storage_state_status()

    # ------------------------------------------------------------------
    # 浏览器操作（需要 Playwright）
    # ------------------------------------------------------------------

    @staticmethod
    def _require_playwright() -> None:
        if not playwright_available():
            raise TransportUnavailableError(_INSTALL_HINT)

    async def login_with_credentials(
        self,
        username: str,
        password: str,
        *,
        login_path: Optional[str] = None,
        extra_fields: Optional[dict[str, str]] = None,
    ) -> None:
        """API 登录并保存 storage state（官方推荐的 APIRequestContext 路径）。

        带验证码/2FA 的平台不适用本方法，应降级为人工辅助登录流程
        （属 COMP-04 GenericBrowserAdapter 的职责）。
        """
        self._require_playwright()
        from playwright.async_api import async_playwright

        self.ensure_profile_dir()
        target = f"{self._base_url}{login_path or self._login_path}"
        async with async_playwright() as pw:
            context = await pw.request.new_context(base_url=self._base_url)
            try:
                form = {"username": username, "password": password}
                form.update(extra_fields or {})
                response = await context.post(target, form=form)
                if response.status in (401, 403):
                    raise PlatformAuthRequiredError(
                        f"browser login failed (HTTP {response.status})",
                        status_code=response.status,
                    )
                if not response.ok:
                    raise PlatformTransportError(
                        f"browser login failed (HTTP {response.status})",
                        status_code=response.status,
                    )
                await context.storage_state(path=str(self.storage_state_path))
            finally:
                await context.dispose()
        os.chmod(self.storage_state_path, 0o600)

    async def probe_session(self) -> None:
        """探测会话有效性；过期/失效抛 ``PlatformAuthRequiredError``。

        判定（核验 §PW 建议）：访问需登录的轻量端点，被 302 到登录页或
        返回 401/403 即视为失效。
        """
        self._require_playwright()
        status = self.storage_state_status()
        if not status["present"]:
            raise PlatformAuthRequiredError("browser session not established")
        if status["expired"]:
            self.clear_storage_state()
            raise PlatformAuthRequiredError("browser session storage state expired")
        from playwright.async_api import async_playwright

        async with async_playwright() as pw:
            context = await pw.request.new_context(
                base_url=self._base_url,
                storage_state=str(self.storage_state_path),
            )
            try:
                response = await context.get(
                    self._probe_path, max_redirects=0
                )
                if response.status in (301, 302, 303, 307, 308):
                    location = response.headers.get("location", "")
                    if "login" in location.lower() or "signin" in location.lower():
                        raise PlatformAuthRequiredError(
                            "browser session expired (redirected to login)"
                        )
                if response.status in (401, 403):
                    raise PlatformAuthRequiredError(
                        f"browser session expired (HTTP {response.status})",
                        status_code=response.status,
                    )
            finally:
                await context.dispose()

    async def api_request(
        self,
        method: str,
        path: str,
        *,
        json_body: Any = None,
        headers: Optional[dict[str, str]] = None,
    ) -> tuple[int, Any]:
        """在授权会话内调用页面同源 JSON API，返回 (status, parsed_body)。

        会话失效抛 ``PlatformAuthRequiredError``；非 JSON 响应抛
        ``PlatformTransportError``（页面结构变化的诊断线索）。
        """
        self._require_playwright()
        if not self.has_storage_state():
            raise PlatformAuthRequiredError("browser session not established")
        from playwright.async_api import async_playwright

        async with async_playwright() as pw:
            context = await pw.request.new_context(
                base_url=self._base_url,
                storage_state=str(self.storage_state_path),
            )
            try:
                request_headers = dict(headers or {})
                if json_body is not None:
                    request_headers.setdefault("Content-Type", "application/json")
                response = await context.fetch(
                    path,
                    method=method,
                    data=json.dumps(json_body) if json_body is not None else None,
                    headers=request_headers or None,
                    max_redirects=0,
                )
                if response.status in (401, 403):
                    raise PlatformAuthRequiredError(
                        f"browser api {method} {path}: auth required "
                        f"(HTTP {response.status})",
                        status_code=response.status,
                    )
                try:
                    body = await response.json()
                except Exception as exc:
                    raise PlatformTransportError(
                        f"browser api {method} {path}: non-JSON response "
                        f"(HTTP {response.status})",
                        status_code=response.status,
                    ) from exc
                return response.status, body
            finally:
                await context.dispose()

    async def text_request(
        self, method: str, path: str
    ) -> tuple[int, str]:
        """在授权会话内读取同源文本页面，供 CSRF 等页面令牌提取。"""
        self._require_playwright()
        if not self.has_storage_state():
            raise PlatformAuthRequiredError("browser session not established")
        from playwright.async_api import async_playwright

        async with async_playwright() as pw:
            context = await pw.request.new_context(
                base_url=self._base_url,
                storage_state=str(self.storage_state_path),
            )
            try:
                response = await context.fetch(
                    path, method=method, max_redirects=0
                )
                if response.status in (401, 403):
                    raise PlatformAuthRequiredError(
                        f"browser page {method} {path}: auth required "
                        f"(HTTP {response.status})",
                        status_code=response.status,
                    )
                return response.status, await response.text()
            finally:
                await context.dispose()

    async def download(self, path: str) -> bytes:
        """经授权会话下载附件（要求活动会话的平台）。"""
        self._require_playwright()
        if not self.has_storage_state():
            raise PlatformAuthRequiredError("browser session not established")
        from playwright.async_api import async_playwright

        async with async_playwright() as pw:
            context = await pw.request.new_context(
                base_url=self._base_url,
                storage_state=str(self.storage_state_path),
            )
            try:
                response = await context.get(path)
                if response.status in (401, 403):
                    raise PlatformAuthRequiredError(
                        f"browser download {path}: auth required "
                        f"(HTTP {response.status})",
                        status_code=response.status,
                    )
                if not response.ok:
                    raise PlatformTransportError(
                        f"browser download {path}: HTTP {response.status}",
                        status_code=response.status,
                    )
                return await response.body()
            finally:
                await context.dispose()


__all__ = [
    "PlaywrightTransport",
    "playwright_available",
]
