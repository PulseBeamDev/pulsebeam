"""Internal participant-token signing profile."""

import base64
import binascii
import json
import re
import unicodedata
import uuid
from dataclasses import dataclass, field
from typing import Literal, TypedDict

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives.serialization import load_der_private_key

_MAX_COMPACT_TOKEN_BYTES = 8192
Action = Literal["join", "publish", "discover", "subscribe"]
TrackKind = Literal["audio", "video", "data"]


class _RequiredGrant(TypedDict):
    rooms: list[str]
    allow: list[Action]


class Grant(_RequiredGrant, total=False):
    participant: str
    kind: TrackKind
    label: str


def _scope(value: object) -> bool:
    if type(value) is not str or not value:
        return False
    try:
        return len(value.encode("utf-8")) <= 256 and not any(
            unicodedata.category(character) == "Cc" for character in value
        )
    except UnicodeEncodeError:
        return False


def _checked_grants(grants: list[Grant]) -> list[Grant]:
    if type(grants) is not list or len(grants) > 32:
        raise ValueError("Invalid grants")
    patterns = 0
    checked = []
    for grant in grants:
        if type(grant) is not dict or set(grant) - {
            "rooms", "allow", "participant", "kind", "label"
        }:
            raise ValueError("Invalid grant")
        rooms = grant.get("rooms")
        allow = grant.get("allow")
        if type(rooms) is not list or not rooms:
            raise ValueError("Invalid rooms")
        patterns += len(rooms)
        if patterns > 64:
            raise ValueError("Invalid room pattern count")
        for room in rooms:
            if not _scope(room) or (
                "*" in room and room != "*"
                and not (room.endswith("/*") and room.count("*") == 1)
            ):
                raise ValueError("Invalid room pattern")
        if type(allow) is not list or not 1 <= len(allow) <= 4 or any(
            type(action) is not str or action not in ("join", "publish", "discover", "subscribe")
            for action in allow
        ) or len(set(allow)) != len(allow):
            raise ValueError("Invalid actions")
        filters = set(grant) & {"participant", "kind", "label"}
        for name in ("participant", "label"):
            if name in filters and not _scope(grant[name]):
                raise ValueError("Invalid grant filter")
        if "kind" in filters and grant["kind"] not in ("audio", "video", "data"):
            raise ValueError("Invalid track kind")
        if "join" in allow and filters:
            raise ValueError("Invalid join filters")
        if "publish" in allow and "participant" in filters:
            raise ValueError("Invalid publish participant filter")
        normalized = {"rooms": list(rooms), "allow": list(allow)}
        for name in ("participant", "kind", "label"):
            if name in filters:
                normalized[name] = grant[name]
        checked.append(normalized)
    return checked


def _base64(value: bytes) -> str:
    return base64.urlsafe_b64encode(value).rstrip(b"=").decode("ascii")


@dataclass(frozen=True)
class SignedParticipantToken:
    token: str = field(repr=False)
    jti: str


def sign_participant_token(
    api_key: str, *, grants: list[Grant], participant: str, expiration: int
) -> SignedParticipantToken:
    """Sign a JWT offline with one opaque API key and absolute Unix expiration.

    No clock, environment lookup, or network request is performed.
    """
    if not isinstance(participant, str) or re.fullmatch(r"[A-Za-z0-9_-]{1,36}", participant) is None:
        raise ValueError("Invalid participant identifier")
    checked = _checked_grants(grants)
    if type(api_key) is not str:
        raise ValueError("Invalid API key")
    parts = api_key.split(".")
    if len(parts) != 3:
        raise ValueError("Invalid API key")
    project, key, secret = parts
    # Let the standard key loader interpret PKCS#8, not a PulseBeam codec.
    try:
        der = base64.urlsafe_b64decode(secret + "=" * (-len(secret) % 4))
        private_key = load_der_private_key(der, password=None)
        if not isinstance(private_key, Ed25519PrivateKey):
            raise ValueError("Invalid API key")
    except (ValueError, TypeError, binascii.Error):
        raise ValueError("Invalid API key") from None
    if type(expiration) is not int or not 0 <= expiration <= (1 << 64) - 1:
        raise ValueError("Invalid expiration")
    header = json.dumps({"alg": "EdDSA", "kid": key, "typ": "pb+jwt"}, separators=(",", ":"))
    jti = str(uuid.uuid4())
    claims = json.dumps(
        {"iss": project, "aud": "pb", "sub": participant, "exp": expiration, "jti": jti, "grants": checked},
        separators=(",", ":"),
        ensure_ascii=False,
    )
    signing_input = _base64(header.encode()) + "." + _base64(claims.encode())
    # Ed25519's 64-byte signature occupies 86 unpadded base64url characters.
    if len(signing_input) + 1 + 86 > _MAX_COMPACT_TOKEN_BYTES:
        raise ValueError("Participant token exceeds the 8192-byte limit")
    signature = private_key.sign(signing_input.encode("ascii"))
    return SignedParticipantToken(signing_input + "." + _base64(signature), jti)
