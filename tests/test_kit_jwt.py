"""muteki_kit.web.jwt — decode / forge none / brute / re-sign."""

from muteki_kit.web import jwt


def test_decode_roundtrip() -> None:
    tok = jwt.sign_hs256({"user": "admin", "role": "user"}, "secret")
    d = jwt.decode(tok)
    assert d.valid_structure
    assert d.alg == "HS256"
    assert d.payload["user"] == "admin"


def test_forge_none_has_empty_signature() -> None:
    tok = jwt.forge_none({"user": "admin", "admin": True})
    assert tok.endswith(".")
    d = jwt.decode(tok)
    assert d.alg == "none"
    assert d.payload["admin"] is True


def test_brute_finds_weak_secret() -> None:
    tok = jwt.sign_hs256({"user": "guest"}, "Sn1f")
    res = jwt.brute_hs256(tok, jwt.COMMON_SECRETS)
    assert res.found and res.secret == "Sn1f"


def test_brute_fails_on_strong_secret() -> None:
    tok = jwt.sign_hs256({"user": "guest"}, "an-extremely-unlikely-secret-99x")
    res = jwt.brute_hs256(tok, jwt.COMMON_SECRETS)
    assert res.found is False
    assert res.tried == len(jwt.COMMON_SECRETS)


def test_resign_after_brute_changes_claims() -> None:
    # attacker recovers secret, forges an admin token
    orig = jwt.sign_hs256({"user": "guest", "admin": False}, "secret")
    rec = jwt.brute_hs256(orig, jwt.COMMON_SECRETS)
    assert rec.found
    forged = jwt.sign_hs256({"user": "guest", "admin": True}, rec.secret)
    d = jwt.decode(forged)
    assert d.payload["admin"] is True
    # and it verifies under the same secret
    again = jwt.brute_hs256(forged, [rec.secret])
    assert again.found


def test_decode_malformed() -> None:
    assert jwt.decode("not.a.jwt.token.x").valid_structure is False
    assert jwt.decode("onlyonepart").valid_structure is False
