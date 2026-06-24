"""muteki_kit crypto track — RSA / classical / symmetric attack helpers.

Pure-pip (gmpy2/sympy/pycryptodome), runs native on macOS. Typed Pydantic
returns + provenance-friendly printing, mirroring the web SDK template.
"""

from muteki_kit.crypto.classical import (
    ClassicalResult,
    caesar_bruteforce,
    vigenere_decrypt,
    xor_bytes,
    xor_known_plaintext,
    xor_repeating_key,
    xor_repeating_key_crack,
    xor_single_byte,
)
from muteki_kit.crypto.rsa import (
    RSABreaker,
    RSAResult,
    common_factor,
    common_modulus,
)
from muteki_kit.crypto.symmetric import (
    SymResult,
    decode_offset_nibbles,
    detect_ecb,
    des_weak_ofb_bruteforce,
    padding_oracle_decrypt,
)

__all__ = [
    "RSABreaker", "RSAResult", "common_factor", "common_modulus",
    "ClassicalResult", "caesar_bruteforce", "vigenere_decrypt",
    "xor_bytes", "xor_single_byte", "xor_repeating_key", "xor_repeating_key_crack",
    "xor_known_plaintext",
    "SymResult", "detect_ecb", "padding_oracle_decrypt", "decode_offset_nibbles",
    "des_weak_ofb_bruteforce",
]
