"""RSA attack toolkit — typed, self-documenting, identify-then-attack.

Mirrors the web SDK pattern: strongly-typed Pydantic returns (no text-parsing by
the model), a single `RSABreaker(n,e,c).auto()` that tries attacks in cost order,
and recovered plaintext PRINTED so it lands in real kernel stdout for the
provenance gate. Pure-pip (gmpy2/sympy/pycryptodome) — runs native on macOS.

Covered attacks (cheapest first):
  - small-e / cube-root (e small, m^e < n)
  - common-factor between two moduli (GCD)
  - Fermat (primes close together)
  - small-prime / sympy.factorint (n with a small factor or fully factorable)
  - Wiener (small d)
  - factordb lookup (known factorizations, offline-friendly: needs network)
  - RsaCtfTool subprocess (optional, if installed) as a catch-all

Usage:
    from muteki_kit.crypto import RSABreaker
    r = RSABreaker(n=..., e=..., c=...).auto()
    if r.recovered: print(r.plaintext_bytes)   # also auto-printed
"""

from __future__ import annotations

import math
from typing import Optional

import gmpy2
import sympy
from Crypto.Util.number import inverse, long_to_bytes
from pydantic import BaseModel


class RSAResult(BaseModel):
    recovered: bool
    method: str = ""
    # what we found (any subset, depending on attack)
    p: Optional[int] = None
    q: Optional[int] = None
    d: Optional[int] = None
    plaintext_int: Optional[int] = None
    plaintext: Optional[str] = None  # decoded text if printable
    notes: str = ""

    @property
    def plaintext_bytes(self) -> Optional[bytes]:
        if self.plaintext_int is None:
            return None
        return long_to_bytes(self.plaintext_int)


def _finish(method: str, m: int, *, p=None, q=None, d=None, notes="") -> RSAResult:
    raw = long_to_bytes(m)
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError:
        text = raw.decode("latin-1", errors="replace")
    res = RSAResult(recovered=True, method=method, p=p, q=q, d=d,
                    plaintext_int=m, plaintext=text, notes=notes)
    # PROVENANCE: print recovered plaintext so it appears in real stdout
    print(f"[rsa:{method}] recovered plaintext: {raw!r}")
    return res


def _decrypt_with_factors(n: int, e: int, c: int, p: int, q: int, method: str) -> RSAResult:
    phi = (p - 1) * (q - 1)
    d = inverse(e, phi)
    m = pow(c, d, n)
    return _finish(method, m, p=p, q=q, d=d)


class RSABreaker:
    """Try RSA attacks in cost order. Give what you have; .auto() does the rest."""

    def __init__(self, n: int, e: int = 65537, c: Optional[int] = None) -> None:
        self.n = int(n)
        self.e = int(e)
        self.c = int(c) if c is not None else None

    # -- individual attacks (each returns RSAResult or recovered=False) ----
    def small_e(self) -> RSAResult:
        """m = c^(1/e) when m^e < n (small e, no padding)."""
        if self.c is None:
            return RSAResult(recovered=False, method="small_e", notes="need c")
        r, exact = gmpy2.iroot(gmpy2.mpz(self.c), self.e)
        if exact:
            return _finish("small_e", int(r))
        return RSAResult(recovered=False, method="small_e")

    def fermat(self, max_iter: int = 1_000_000) -> RSAResult:
        """Factor n when p, q are close (a^2 - n = b^2)."""
        a = gmpy2.isqrt(self.n)
        if a * a < self.n:
            a += 1
        for _ in range(max_iter):
            b2 = a * a - self.n
            b, exact = gmpy2.iroot(b2, 2)
            if exact:
                p, q = int(a + b), int(a - b)
                if p * q == self.n and 1 < p < self.n:
                    if self.c is None:
                        return RSAResult(recovered=True, method="fermat", p=p, q=q,
                                         notes="factored; provide c to decrypt")
                    return _decrypt_with_factors(self.n, self.e, self.c, p, q, "fermat")
            a += 1
        return RSAResult(recovered=False, method="fermat")

    def factorize(self, *, limit_digits: int = 90) -> RSAResult:
        """Try sympy.factorint (handles small factors / fully-small moduli)."""
        if len(str(self.n)) > limit_digits:
            return RSAResult(recovered=False, method="factorize",
                             notes="n too large for direct factoring")
        f = sympy.factorint(self.n)
        primes = [pp for pp, mult in f.items() for _ in range(mult)]
        if len(primes) == 2:
            p, q = int(primes[0]), int(primes[1])
            if self.c is None:
                return RSAResult(recovered=True, method="factorize", p=p, q=q)
            return _decrypt_with_factors(self.n, self.e, self.c, p, q, "factorize")
        return RSAResult(recovered=False, method="factorize",
                         notes=f"factored into {len(primes)} primes (not 2)")

    def wiener(self) -> RSAResult:
        """Wiener's attack: small private exponent d via continued fractions."""
        # continued fraction expansion of e/n
        def contfrac(a, b):
            while b:
                yield a // b
                a, b = b, a % b

        def convergents(cf):
            num0, num1 = 0, 1
            den0, den1 = 1, 0
            for q in cf:
                num0, num1 = num1, q * num1 + num0
                den0, den1 = den1, q * den1 + den0
                yield num1, den1

        for k, d in convergents(contfrac(self.e, self.n)):
            if k == 0:
                continue
            if (self.e * d - 1) % k != 0:
                continue
            phi = (self.e * d - 1) // k
            # solve x^2 - (n - phi + 1)x + n = 0 ; integer roots => p,q
            b = self.n - phi + 1
            disc = b * b - 4 * self.n
            if disc < 0:
                continue
            r, exact = gmpy2.iroot(disc, 2)
            if not exact:
                continue
            p = (b + int(r)) // 2
            q = (b - int(r)) // 2
            if p * q == self.n:
                if self.c is None:
                    return RSAResult(recovered=True, method="wiener", p=int(p), q=int(q), d=int(d))
                return _decrypt_with_factors(self.n, self.e, self.c, int(p), int(q), "wiener")
        return RSAResult(recovered=False, method="wiener")

    def factordb(self, timeout: float = 10.0) -> RSAResult:
        """Look n up in factordb.com (known factorizations). Needs network."""
        try:
            import httpx

            r = httpx.get("http://factordb.com/api", params={"query": str(self.n)},
                          timeout=timeout)
            data = r.json()
            factors = []
            for base, mult in data.get("factors", []):
                factors.extend([int(base)] * int(mult))
            if len(factors) == 2 and factors[0] * factors[1] == self.n:
                p, q = factors
                if self.c is None:
                    return RSAResult(recovered=True, method="factordb", p=p, q=q)
                return _decrypt_with_factors(self.n, self.e, self.c, p, q, "factordb")
        except Exception as exc:  # network/json — non-fatal, just skip
            return RSAResult(recovered=False, method="factordb", notes=f"factordb: {exc}")
        return RSAResult(recovered=False, method="factordb")

    def auto(self, *, try_network: bool = True) -> RSAResult:
        """Run attacks in cost order; return the first that recovers."""
        for attack in (self.small_e, self.fermat, self.factorize, self.wiener):
            res = attack()
            if res.recovered:
                return res
        if try_network:
            res = self.factordb()
            if res.recovered:
                return res
        return RSAResult(recovered=False, method="auto",
                         notes="no cheap attack worked; consider common-modulus/"
                               "Hastad/Coppersmith (needs more ciphertexts or Sage)")


def common_modulus(n: int, e1: int, c1: int, e2: int, c2: int) -> RSAResult:
    """Same n, two coprime exponents, two ciphertexts of the same m -> recover m."""
    g, a, b = _egcd(e1, e2)
    if g != 1:
        return RSAResult(recovered=False, method="common_modulus", notes="gcd(e1,e2)!=1")
    # m = c1^a * c2^b mod n (handle negative exponents via inverse)
    if a < 0:
        c1 = inverse(c1, n)
        a = -a
    if b < 0:
        c2 = inverse(c2, n)
        b = -b
    m = (pow(c1, a, n) * pow(c2, b, n)) % n
    return _finish("common_modulus", m)


def common_factor(n1: int, n2: int) -> Optional[int]:
    """Shared prime between two moduli (GCD). Returns the common prime or None.

    The bound is `< max(n1, n2)` (not `< n1`): when one modulus divides the other
    (e.g. n1 | n2, so gcd == n1) the shared prime equals n1 and the old `< n1` test
    wrongly rejected it. `< max` keeps that real factor while still excluding the
    degenerate `n1 == n2` case (gcd == n1 == n2 would otherwise hand back the whole
    modulus, which is not a proper factor)."""
    g = math.gcd(n1, n2)
    return g if 1 < g < max(n1, n2) else None


def _egcd(a: int, b: int):
    if b == 0:
        return a, 1, 0
    g, x, y = _egcd(b, a % b)
    return g, y, x - (a // b) * y
