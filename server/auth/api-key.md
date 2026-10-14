# V0 API key format

The private server-side credential is carried by a **single** configuration
value, `PULSEBEAM_API_KEY`. Its canonical string form is:

```text
key_0<103 Crockford Base32 characters>
```

`key` identifies a private API key. `_` separates the type from a single
format-version digit. `0` is **V0**; unknown versions must be rejected.
The complete V0 string is exactly **108 ASCII characters**. It is **not** a JWT,
not a participant access token, and not safe to expose to the browser.

## Binary layout (64 bytes, exactly)

| Offset | Length | Contents |
| --- | ---: | --- |
| 0 | 16 bytes | `project_id` — RFC 4122 UUIDv7 bytes |
| 16 | 16 bytes | `key_id` — RFC 4122 UUIDv7 bytes |
| 32 | 32 bytes | `seed` — raw Ed25519 signing seed |

UUIDs are stored in canonical network/big-endian byte order, without their
`p_0` or `kid_0` textual prefixes. The seed is the **32-byte seed**, not
an expanded Ed25519 private key, a PEM document, or a public key. The API key
is a lossless packaging of the existing project ID, API key ID and signing
secret; it does **not** alter key generation, JWT signing, registry lookup, or
verification. The public registry continues to contain only project ID, key ID,
and Ed25519 public verification key.

Encode the whole 64-byte payload as **one** MSB-first Crockford Base32 stream:
`0123456789ABCDEFGHJKMNPQRSTVWXYZ`. This yields exactly 103 characters.
The last encoded digit contains **three zero low padding bits**; a decoder
must reject nonzero padding or an incorrect length. The canonical form uses
uppercase digits and the ASCII `0` version marker. As with PulseBeam's
existing ID codecs, decoding may accept lowercase and the Crockford aliases
`O/o → 0`, `I/i/L/l → 1`, and always re-emits canonical uppercase.
Reject Unicode, invalid Base32 characters, UUIDs without RFC 4122 variant
and UUIDv7 version, and unknown credential versions. Never echo the API key
or decoded seed in errors, diagnostics, or logs.

## Development-only golden value

This public example corresponds **exactly** to the canonical
[development.json](development.json) fixture. It is unsafe for production.

```text
key_00000000001R01000000000000400000000070040000000000019TRDHKQQZTPK0QA24NX4JXGPC8H29RNMQPCK935R3QB033JQ7YR0
```

It decodes to:
- `project_id`: `p_00000000001R010000000000004`
- `key_id`: `kid_00000000001R010000000000008`
- `seed` (hex): `9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60`

## Token issuance, lifecycle, and future versions

The backend parses the API key and signs a separate, short-lived **participant
JWT**. The existing JWT header `kid`, claims `iss`, `aud`, `sub`, `room`,
`exp`, and EdDSA signature are unchanged. Only that JWT goes to the browser.
Keep `PULSEBEAM_API_KEY` on the server; treat the entire string as sensitive.
Issue a new API key with a new `key_id` and seed to rotate keys, register its
public key, and retire the previous public key after outstanding JWTs expire.

The V0 `key_0` payload is permanently the 16 + 16 + 32 byte structure above.
If another algorithm, key length, or metadata becomes necessary, define
`key_1` with a new, documented layout. Never silently reinterpret V0.
