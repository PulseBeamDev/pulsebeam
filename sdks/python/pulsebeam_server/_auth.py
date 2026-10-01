"""Internal participant-token signing profile."""

import base64
import json

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

_ALPHABET = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"
_DIGITS = {c: i for i, c in enumerate(_ALPHABET)}
_DIGITS.update({"O": 0, "I": 1, "L": 1})


def _payload(value: str, prefix: str, length: int) -> str:
    if (
        type(value) is not str
        or len(value) != len(prefix) + 2 + length
        or not value.startswith(prefix + "_")
        or value[len(prefix) + 1] not in "0Oo"
    ):
        raise ValueError("Invalid credential format")
    return value[len(prefix) + 2 :]


def _decode(text: str) -> int:
    n = 0
    for c in text:
        # Reject non-ASCII before Unicode case folding can introduce aliases.
        if not c.isascii() or c.upper() not in _DIGITS:
            raise ValueError("Invalid credential encoding")
        n = (n << 5) | _DIGITS[c.upper()]
    return n


def _canonical_id(value: str, prefix: str) -> str:
    n = _decode(_payload(value, prefix, 26))
    if n & 3:
        raise ValueError("Invalid credential ID padding")
    uuid = (n >> 2).to_bytes(16, "big")
    if uuid[6] >> 4 != 7 or uuid[8] & 0xC0 != 0x80:
        raise ValueError("Invalid credential UUID")
    canonical = ""
    for _ in range(26):
        canonical = _ALPHABET[n & 31] + canonical
        n >>= 5
    return prefix + "_0" + canonical


def _external(value: str) -> bool:
    return (
        type(value) is str
        and 1 <= len(value) <= 36
        and all(c in "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-" for c in value)
    )


def _base64(value: bytes) -> str:
    return base64.urlsafe_b64encode(value).rstrip(b"=").decode("ascii")


def sign_participant_token(
    *, project_id: str, key_id: str, secret: str, room: str, participant: str, expiration: int
) -> str:
    """Sign a JWT offline with explicit credentials and absolute Unix expiration.

    All inputs are mandatory. expiration is an exact unsigned 64-bit int, not a
    bool, float, string, or TTL. No clock or environment lookup is performed.
    """
    project = _canonical_id(project_id, "p")
    key = _canonical_id(key_id, "kid")
    seed = _decode(_payload(secret, "sk", 52))
    if seed >= 1 << 256:
        raise ValueError("Invalid signing secret padding")
    if not _external(room) or not _external(participant):
        raise ValueError("Invalid room or participant external ID")
    if type(expiration) is not int or not 0 <= expiration <= (1 << 64) - 1:
        raise ValueError("Invalid expiration")
    header = json.dumps({"alg": "EdDSA", "kid": key, "typ": "pb+jwt"}, separators=(",", ":"))
    claims = json.dumps(
        {"iss": project, "aud": "pb", "sub": participant, "room": room, "exp": expiration},
        separators=(",", ":"),
    )
    signing_input = _base64(header.encode()) + "." + _base64(claims.encode())
    signature = Ed25519PrivateKey.from_private_bytes(seed.to_bytes(32, "big")).sign(signing_input.encode("ascii"))
    return signing_input + "." + _base64(signature)
