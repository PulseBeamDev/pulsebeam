"""Internal participant-token signing profile."""

import base64
import binascii
import json

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

_ALPHABET = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"
_STANDARD = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567"
_TO_STANDARD = str.maketrans(_ALPHABET, _STANDARD)
_FROM_STANDARD = str.maketrans(_STANDARD, _ALPHABET)
_ALIASES = str.maketrans("OIL", "011")


def _payload(value: str, prefix: str, length: int) -> str:
    if (
        type(value) is not str
        or len(value) != len(prefix) + 2 + length
        or not value.startswith(prefix + "_")
        or value[len(prefix) + 1] not in "0Oo"
    ):
        raise ValueError("Invalid credential format")
    return value[len(prefix) + 2 :]


def _encode(value: bytes) -> str:
    return base64.b32encode(value).decode("ascii").rstrip("=").translate(_FROM_STANDARD)


def _decode(text: str) -> bytes:
    if not text.isascii():
        raise ValueError("Invalid credential encoding")
    normalized = text.upper().translate(_ALIASES)
    if any(c not in _ALPHABET for c in normalized):
        raise ValueError("Invalid credential encoding")
    try:
        decoded = base64.b32decode(normalized.translate(_TO_STANDARD) + "=" * (-len(normalized) % 8))
    except binascii.Error:
        raise ValueError("Invalid credential encoding") from None
    # Python 3.10 accepts nonzero unused bits; the profile requires canonical bytes.
    if _encode(decoded) != normalized:
        raise ValueError("Invalid credential padding")
    return decoded


def _canonical_id(value: str, prefix: str) -> str:
    uuid = _decode(_payload(value, prefix, 26))
    if uuid[6] >> 4 != 7 or uuid[8] & 0xC0 != 0x80:
        raise ValueError("Invalid credential UUID")
    return prefix + "_0" + _encode(uuid)


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
    # Twenty added zero bits align the seed's four high padding bits to three bytes.
    padded_seed = _decode("0000" + _payload(secret, "sk", 52))
    if padded_seed[:3] != b"\x00\x00\x00":
        raise ValueError("Invalid signing secret padding")
    seed = padded_seed[3:]
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
    signature = Ed25519PrivateKey.from_private_bytes(seed).sign(signing_input.encode("ascii"))
    return signing_input + "." + _base64(signature)
