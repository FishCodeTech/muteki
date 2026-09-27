"""扩展 Catalog 的 Ed25519 发布者签名验证。"""

from __future__ import annotations

import base64
import hashlib

Q = 2**255 - 19
L = 2**252 + 27742317777372353535851937790883648493
D = (-121665 * pow(121666, Q - 2, Q)) % Q
SQRT_M1 = pow(2, (Q - 1) // 4, Q)


class SignatureError(ValueError):
    """公钥、签名编码或 Ed25519 验证失败。"""


def _xrecover(y: int) -> int:
    xx = (y * y - 1) * pow(D * y * y + 1, Q - 2, Q) % Q
    x = pow(xx, (Q + 3) // 8, Q)
    if (x * x - xx) % Q:
        x = x * SQRT_M1 % Q
    if (x * x - xx) % Q:
        raise SignatureError("invalid Ed25519 point")
    return Q - x if x & 1 else x


BY = 4 * pow(5, Q - 2, Q) % Q
BASE = (_xrecover(BY), BY)
IDENTITY = (0, 1)


def _add(left: tuple[int, int], right: tuple[int, int]) -> tuple[int, int]:
    x1, y1 = left
    x2, y2 = right
    product = D * x1 * x2 * y1 * y2 % Q
    return (
        (x1 * y2 + x2 * y1) * pow(1 + product, Q - 2, Q) % Q,
        (y1 * y2 + x1 * x2) * pow(1 - product, Q - 2, Q) % Q,
    )


def _multiply(point: tuple[int, int], scalar: int) -> tuple[int, int]:
    result = IDENTITY
    current = point
    value = scalar
    while value:
        if value & 1:
            result = _add(result, current)
        current = _add(current, current)
        value >>= 1
    return result


def _decode_point(raw: bytes) -> tuple[int, int]:
    if len(raw) != 32:
        raise SignatureError("Ed25519 point must be 32 bytes")
    encoded = int.from_bytes(raw, "little")
    sign = encoded >> 255
    y = encoded & ((1 << 255) - 1)
    if y >= Q:
        raise SignatureError("non-canonical Ed25519 point")
    x = _xrecover(y)
    if (x & 1) != sign:
        x = Q - x
    point = (x, y)
    if _multiply(point, L) != IDENTITY or point == IDENTITY:
        raise SignatureError("Ed25519 point is outside the prime-order subgroup")
    return point


def _encode_point(point: tuple[int, int]) -> bytes:
    x, y = point
    return (y | ((x & 1) << 255)).to_bytes(32, "little")


def _decode_text(value: str, expected: int) -> bytes:
    text = str(value or "").strip()
    try:
        raw = bytes.fromhex(text) if len(text) == expected * 2 else base64.b64decode(
            text, validate=True)
    except (ValueError, TypeError) as exc:
        raise SignatureError("signature material must be hex or base64") from exc
    if len(raw) != expected:
        raise SignatureError(f"signature material must be {expected} bytes")
    return raw


def verify_ed25519(public_key: str, signature: str, message: bytes) -> bool:
    """验证 Ed25519 签名；输入支持 hex 与标准 base64。"""
    public_raw = _decode_text(public_key, 32)
    signature_raw = _decode_text(signature, 64)
    public_point = _decode_point(public_raw)
    r_point = _decode_point(signature_raw[:32])
    scalar = int.from_bytes(signature_raw[32:], "little")
    if scalar >= L:
        raise SignatureError("non-canonical Ed25519 scalar")
    challenge = int.from_bytes(
        hashlib.sha512(signature_raw[:32] + public_raw + message).digest(),
        "little",
    ) % L
    return _multiply(BASE, scalar) == _add(
        r_point, _multiply(public_point, challenge))


def public_key_from_seed(seed: str) -> str:
    """从 32 字节 Ed25519 私钥种子导出 base64 公钥。"""
    seed_raw = _decode_text(seed, 32)
    digest = hashlib.sha512(seed_raw).digest()
    scalar_raw = bytearray(digest[:32])
    scalar_raw[0] &= 248
    scalar_raw[31] &= 63
    scalar_raw[31] |= 64
    scalar = int.from_bytes(scalar_raw, "little")
    return base64.b64encode(_encode_point(_multiply(BASE, scalar))).decode()


def sign_ed25519(seed: str, message: bytes) -> str:
    """使用 32 字节 Ed25519 私钥种子签名，返回标准 base64。"""
    seed_raw = _decode_text(seed, 32)
    digest = hashlib.sha512(seed_raw).digest()
    scalar_raw = bytearray(digest[:32])
    scalar_raw[0] &= 248
    scalar_raw[31] &= 63
    scalar_raw[31] |= 64
    scalar = int.from_bytes(scalar_raw, "little")
    public = _encode_point(_multiply(BASE, scalar))
    nonce = int.from_bytes(
        hashlib.sha512(digest[32:] + message).digest(), "little") % L
    encoded_r = _encode_point(_multiply(BASE, nonce))
    challenge = int.from_bytes(
        hashlib.sha512(encoded_r + public + message).digest(), "little") % L
    signature = encoded_r + ((nonce + challenge * scalar) % L).to_bytes(32, "little")
    return base64.b64encode(signature).decode()


__all__ = [
    "SignatureError",
    "public_key_from_seed",
    "sign_ed25519",
    "verify_ed25519",
]
