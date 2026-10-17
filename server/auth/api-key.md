# PulseBeam API key (SDK maintainers)

Applications treat an API key as an opaque server-side credential. The CLI
generates it; each server SDK signs participant JWTs locally.

## Credential format

```text
<ProjectId>.<ApiKeyId>.<base64url(PKCS#8 DER)>
```

The first two components retain the canonical PulseBeam Crockford ID formats:
`p_0<26 Crockford digits>` and `kid_0<26 Crockford digits>`.
The third is **unpadded base64url of an Ed25519 PKCS#8 DER private key**
(RFC 8410), not a proprietary `sk_0` seed representation.
CLI-generated minimal PKCS#8 DER is 48 bytes / 64 base64url characters,
making the compound credential **126 characters**. Other valid PKCS#8
representations can be larger.

Server SDKs split exactly three fields, copy the first two *without
interpreting them* into JWT `iss` and `kid`, and import the third using
their native PKCS#8 crypto library. Keys must be Ed25519 to sign `EdDSA`.
No SDK needs a PulseBeam identity/key codec. Errors must not expose secrets.

## Public registry

```json
{
  "projectId": "p_0...",
  "keys": [
    {"keyId": "kid_0...", "publicKey": "<base64url(SPKI DER)>"}
  ]
}
```

The publicKey value is unpadded base64url of **SubjectPublicKeyInfo (SPKI) DER**
(RFC 8410), not a custom `pk_0` string. Minimal Ed25519 SPKI is
44 bytes / 59 base64url characters. SPKI contains no private key or certificate.
The Rust auth layer imports and verifies these keys against the trusted
project-scoped registry.

## Verification and trust

The JWT header and claims are unchanged:

```text
{"alg":"EdDSA","kid":"<ApiKeyId>","typ":"pb+jwt"}
{"iss":"<ProjectId>","aud":"pb","sub":"<participant>","room":"<room>","exp":2000000000}
```

The SFU validates issuer/key identifiers against the configured project,
selects its registered public key, requires EdDSA, verifies the signature,
validates room/participant identity and expiration, then enforces authorization.
Only the participant JWT is sent to the browser.

Key rotation creates a fresh `kid_0` and key pair, adds its SPKI key to the
registry, and keeps the old public key registered until outstanding tokens
expire. Existing `key_0`, `sk_0` and `pk_0` credentials and registries must be
regenerated; there is no automatic legacy-format fallback.

The embedded [development fixture](development.json) is public and **never
safe for production or an exposed development server**.
