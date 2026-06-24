"""muteki_kit.web.sqli.blind_extract — one-call blind boolean SQLi extraction.

Uses an in-memory oracle that answers SQL boolean conditions against a known
secret, proving the helper resolves LENGTH + each char correctly (and cheaply).
"""

import re

from muteki_kit.web.sqli import blind_extract, boolean_oracle_from_response


def _make_oracle(secret: str):
    """An oracle that truthfully answers LENGTH(...)>n and ASCII(SUBSTRING...)>n."""
    def oracle(cond: str) -> bool:
        m = re.match(r"LENGTH\(.*\)>(\d+)", cond)
        if m:
            return len(secret) > int(m.group(1))
        m = re.match(r"ASCII\(SUBSTRING\(.*,(\d+),1\)\)>(\d+)", cond)
        if m:
            idx, n = int(m.group(1)), int(m.group(2))
            return ord(secret[idx - 1]) > n
        m = re.match(r"ASCII\(SUBSTRING\(.*,(\d+),1\)\)=(\d+)", cond)
        if m:
            idx, n = int(m.group(1)), int(m.group(2))
            return ord(secret[idx - 1]) == n
        raise AssertionError(f"unexpected cond: {cond}")
    return oracle


def test_blind_extract_recovers_secret_bisect() -> None:
    secret = "S3cr3t_p4ss!"
    r = blind_extract(_make_oracle(secret), "(SELECT password FROM users LIMIT 1)")
    assert r.value == secret
    assert r.complete
    # binary search: ~7 reqs/char + length search, far fewer than 95*len linear
    assert r.requests < (len(secret) + 1) * 12


def test_blind_extract_stops_at_flag_terminator() -> None:
    secret = "flag{abc}tail_ignored"
    r = blind_extract(_make_oracle(secret), "(SELECT x)")
    assert r.value == "flag{abc}"  # stopped at the closing brace


def test_oracle_builder() -> None:
    # send() returns a body; truthy when marker present
    def send(cond: str) -> str:
        return "WELCOME" if "1=1" in cond else "DENIED"
    o = boolean_oracle_from_response(send, "WELCOME")
    assert o("1=1") is True
    assert o("1=2") is False


def test_extract_by_char_test_owns_all_sql() -> None:
    """The flexible interface: caller provides char_gt/length_gt, helper just
    binary-searches — works with any oracle shape."""
    from muteki_kit.web.sqli import extract_by_char_test
    secret = "Adm1n_Pw!"
    def char_gt(i, n):
        return ord(secret[i - 1]) > n
    def length_gt(n):
        return len(secret) > n
    r = extract_by_char_test(char_gt, length_gt)
    assert r.value == secret
    assert r.complete
