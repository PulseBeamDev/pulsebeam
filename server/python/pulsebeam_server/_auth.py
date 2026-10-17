"""Internal participant-token signing profile."""

import base64
import binascii
import json

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives.serialization import load_der_private_key

def _base64(value: bytes) -> str:
    return base64.urlsafe_b64encode(value).rstrip(b"=").decode("ascii")


def sign_participant_token(
    api_key: str, *, room: str, participant: str, expiration: int
) -> str:
    """Sign a JWT offline with one opaque API key and absolute Unix expiration.

    No clock, environment lookup, or network request is performed.
    """
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
    claims = json.dumps(
        {"iss": project, "aud": "pb", "sub": participant, "room": room, "exp": expiration},
        separators=(",", ":"),
    )
    signing_input = _base64(header.encode()) + "." + _base64(claims.encode())
    signature = private_key.sign(signing_input.encode("ascii"))
    return signing_input + "." + _base64(signature)
