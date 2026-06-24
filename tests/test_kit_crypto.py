"""muteki_kit.crypto — RSA / classical / symmetric attacks (pure math, no network).

Recovers plaintext on classic vulnerable parameter sets, proving the typed-SDK
pattern generalizes off the web track. Runs in normal CI (no Docker, no API).
"""

import sympy
from Crypto.Util.number import bytes_to_long, getPrime, inverse, long_to_bytes

from muteki_kit.crypto import (
    RSABreaker,
    caesar_bruteforce,
    common_factor,
    common_modulus,
    detect_ecb,
    padding_oracle_decrypt,
    decode_offset_nibbles,
    des_weak_ofb_bruteforce,
    xor_known_plaintext,
    xor_repeating_key,
    xor_repeating_key_crack,
    xor_single_byte,
)

FLAG = b"flag{cryptography_is_fun}"
M = bytes_to_long(FLAG)


def test_rsa_small_e() -> None:
    e = 3
    # n large enough that m^e < n is false unless we pick big primes; use a tiny m
    p, q = getPrime(512), getPrime(512)
    n = p * q
    m = bytes_to_long(b"hi")  # tiny so m^3 < n
    c = pow(m, e, n)
    r = RSABreaker(n, e, c).small_e()
    assert r.recovered and r.plaintext_bytes == b"hi"


def test_rsa_fermat_close_primes() -> None:
    p = sympy.nextprime(2**256)
    q = sympy.nextprime(p + 1000)  # very close -> Fermat factors fast
    n = p * q
    e = 65537
    c = pow(M, e, n)
    r = RSABreaker(n, e, c).fermat()
    assert r.recovered and r.plaintext_bytes == FLAG
    assert r.method == "fermat"


def test_rsa_wiener_small_d() -> None:
    # construct a Wiener-vulnerable key: small d, derive e
    p, q = getPrime(512), getPrime(512)
    n = p * q
    phi = (p - 1) * (q - 1)
    d = 0
    for cand in range(3, 2**18, 2):
        if sympy.gcd(cand, phi) == 1 and cand < (n ** 0.25) / 3:
            d = cand
            break
    assert d
    e = inverse(d, phi)
    c = pow(M, e, n)
    r = RSABreaker(n, e, c).wiener()
    assert r.recovered and r.plaintext_bytes == FLAG


def test_rsa_factorize_small_modulus() -> None:
    p, q = sympy.nextprime(10**20), sympy.nextprime(10**21)  # clearly distinct
    n = p * q
    assert p != q
    e = 65537
    small_m = bytes_to_long(b"hi!")  # must be < n (~2^135)
    c = pow(small_m, e, n)
    r = RSABreaker(n, e, c).factorize()
    assert r.recovered and r.plaintext_bytes == b"hi!"


def test_rsa_auto_picks_an_attack() -> None:
    p = sympy.nextprime(2**256)
    q = sympy.nextprime(p + 500)
    n = p * q
    c = pow(M, 65537, n)
    r = RSABreaker(n, 65537, c).auto(try_network=False)
    assert r.recovered and r.plaintext_bytes == FLAG


def test_common_modulus() -> None:
    p, q = getPrime(512), getPrime(512)
    n = p * q
    e1, e2 = 3, 65537  # coprime
    c1, c2 = pow(M, e1, n), pow(M, e2, n)
    r = common_modulus(n, e1, c1, e2, c2)
    assert r.recovered and r.plaintext_bytes == FLAG


def test_common_factor() -> None:
    p = getPrime(512)
    n1, n2 = p * getPrime(512), p * getPrime(512)
    assert common_factor(n1, n2) == p


def test_common_factor_when_one_divides_the_other() -> None:
    """#19 regression: when n1 | n2 the gcd equals n1 itself — the old `g < n1`
    bound wrongly rejected this real shared prime. `common_factor(15, 45)` must be 15
    (15 | 45), not None."""
    assert common_factor(15, 45) == 15
    assert common_factor(45, 15) == 15  # order-independent (uses max)
    # realistic: n1 is a prime p, n2 = p * q → gcd == p == n1
    p = getPrime(256)
    n2 = p * getPrime(256)
    assert common_factor(p, n2) == p


def test_common_factor_equal_moduli_is_none() -> None:
    """Degenerate guard: identical moduli must NOT hand back the whole modulus as a
    'factor' (gcd == n1 == n2 is not a proper shared prime)."""
    assert common_factor(77, 77) is None
    assert common_factor(1, 1) is None


def test_xor_single_byte() -> None:
    key = 0x42
    data = bytes(c ^ key for c in b"the quick brown fox jumps over")
    r = xor_single_byte(data)
    assert r.recovered and "quick brown fox" in r.plaintext


def test_xor_repeating_key() -> None:
    from muteki_kit.crypto import xor_bytes
    pt = b"flag{repeating_xor_key_recovered}"
    ct = xor_bytes(pt, b"KEY")
    r = xor_repeating_key(ct, b"KEY")
    assert r.plaintext == pt.decode()


def test_xor_repeating_key_crack() -> None:
    from muteki_kit.crypto import xor_bytes
    pt = (
        b"flag{automatic_repeating_xor_recovery} "
        b"the quick brown fox jumps over the lazy dog " * 4
    )
    ct = xor_bytes(pt, b"ICE")
    r = xor_repeating_key_crack(ct, max_key_len=12)
    assert r.recovered
    assert "automatic_repeating_xor_recovery" in r.plaintext


def test_xor_known_plaintext() -> None:
    from muteki_kit.crypto import xor_bytes
    stream = b"stream"
    known = b"known plaintext"
    known_ct = xor_bytes(known, stream)
    target = xor_bytes(b"flag{known_stream}", stream)
    r = xor_known_plaintext(known_ct, known, target=target, repeat=True)
    assert r.recovered
    assert "flag{known_stream}" in r.plaintext


def test_caesar() -> None:
    # ROT13 of "flag{caesar}" — caesar_bruteforce finds the shift with '{'
    enc = "synt{pnrfne}"
    r = caesar_bruteforce(enc)
    assert r.recovered and r.plaintext == "flag{caesar}"


def test_detect_ecb() -> None:
    block = b"A" * 16
    ct = block + block + b"different_block!"  # two identical blocks
    r = detect_ecb(ct)
    assert r.recovered  # ECB detected


def test_padding_oracle() -> None:
    # build a real CBC padding oracle over AES and recover the plaintext
    from Crypto.Cipher import AES
    from Crypto.Util.Padding import pad, unpad

    key = b"0123456789abcdef"
    iv = b"fedcba9876543210"
    secret = b"flag{padding_oracle_pwned}"
    ct = AES.new(key, AES.MODE_CBC, iv).encrypt(pad(secret, 16))

    def oracle(two_blocks: bytes) -> bool:
        # two_blocks = forged_prev(16) + target(16); valid padding?
        prev, target = two_blocks[:16], two_blocks[16:32]
        dec = AES.new(key, AES.MODE_ECB).decrypt(target)
        pt = bytes(a ^ b for a, b in zip(dec, prev))
        try:
            unpad(pt, 16)
            return True
        except ValueError:
            return False

    r = padding_oracle_decrypt(oracle, ct, iv)
    assert r.recovered and r.plaintext == secret


def test_decode_offset_nibbles() -> None:
    plain = b"\x01\xabflag"
    offset = 9133337
    width = len(str(offset)) + 1
    vals = {**{str(i): i for i in range(10)}, **{ch: i + 11 for i, ch in enumerate("abcdef")}}
    enc = "".join(str(vals[ch] + offset).rjust(width, "0") for ch in plain.hex())
    r = decode_offset_nibbles(enc, offset)
    assert r.recovered and r.plaintext == plain


def test_des_weak_ofb_bruteforce_known_plaintext() -> None:
    from Crypto.Cipher import DES
    key1 = bytes.fromhex("0101010101010101")
    key2 = bytes.fromhex("FEFEFEFEFEFEFEFE")
    iv = b"13371337"
    known = b"known plaintext block"
    if len(known) % 8:
        known_padded = known + b"_" * (8 - len(known) % 8)
    else:
        known_padded = known
    target = b"flag{weak_des_ofb}"
    if len(target) % 8:
        target_padded = target + b"_" * (8 - len(target) % 8)
    else:
        target_padded = target

    def enc2(data: bytes) -> bytes:
        mid = DES.new(key1, DES.MODE_OFB, iv=iv).encrypt(data)
        return DES.new(key2, DES.MODE_OFB, iv=iv).encrypt(mid)

    r = des_weak_ofb_bruteforce(
        enc2(target_padded),
        iv=iv,
        known_plaintext=known,
        known_ciphertext=enc2(known_padded),
    )
    assert r.recovered
    assert r.key == f"{key1.hex()}:{key2.hex()}"
    assert r.plaintext.startswith(target)
