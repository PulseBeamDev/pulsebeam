# Participant-token conformance profile, version 2

This language-neutral profile and [vectors.json](vectors.json) are the SDK
correctness authority. The existing Rust server remains the authorization
boundary. No SDK or vector generator can alter server acceptance rules.

The shared Rust [`pulsebeam-auth`](../../crates/pulsebeam-auth/README.md) library
owns transport-free ID/key codecs. Core and the Rust SDK depend on it separately:
the SDK owns JWT generation, while core owns registry matching and verification.
This conformance package tests their boundary; it is not the shared library or a
public signing package.

## Shared development identity

The public development identity is defined in [development.json](development.json).
Rust core embeds it directly. Bazel stages the same file into each independently
distributed server SDK's package. No credential copies are committed or
synchronization step is needed.

These keys are publicly known: **never use them for production or an exposed server**.

## Server API keys

Applications pass one opaque API key explicitly to their backend SDK.
The SDK only splits the credential and imports its standard PKCS#8 private key;
it does not validate or reconstruct identifiers. The SFU owns identity
validation and authorization. Applications must not parse or depend on the
credential's representation.
Participant JWTs carry grants, not a selected room or role. Legacy room-bound
JWTs are rejected. The public SDKs do not accept separate project IDs, key IDs,
or signing secrets.

## Inputs and codecs

Public signing inputs are one opaque API key, grants, participant ID,
and absolute expiration in Unix seconds. The API key is a dot-separated
triple of canonical `ProjectId.ApiKeyId` strings and a base64url PKCS#8 private key
(see [the internal format specification](api-key.md)). Server SDKs pass the
already-formatted identities into the JWT and import a standard Ed25519 private key.
Expiration is an exact unsigned integer in `0..18446744073709551615`. No clock,
TTL, normalization of participant IDs, or service lookup is involved.
Zero and past expiration can be signed, but authorize nothing at or after expiry.

Each issuance, including renewal, generates a fresh UUIDv4 using a cryptographic
random source. The SDK returns the compact token and its signed `jti`. Send only
the token to the client. Keep `jti` with your app session if you need incident
correlation; do not log the JWT. Reissue any older tokens that lack `jti`.

Participant IDs match `[A-Za-z0-9_-]{1,36}` and preserve case. Grant room
patterns and filters follow the rules below. Project and key IDs have exact
case-sensitive `p_` and `kid_` prefixes,
followed by V0 marker (`0`, `O`, or `o`) and 26 Crockford Base32 characters. Encode
UUID bytes in big-endian bit order, with two zero **low** padding bits. Require
UUID v7 (high nibble of byte 6 equals 7) and RFC4122 variant (byte 8 high bits 10).
Emit canonical uppercase payload and `0` marker.

Private keys use PKCS#8 DER, and public registry keys use SPKI DER (RFC 8410),
both encoded as unpadded base64url. Standard crypto libraries import the key
representations; the SDKs do not implement Crockford signing-key codecs.
Project and key IDs remain canonical PulseBeam Crockford UUIDs, validated
by the Rust auth/SFU layer.

## Grants

Grants are additive. Each object contains nonempty `rooms` and `allow` arrays:

```json
[
  { "rooms": ["workspace/*"], "allow": ["join"] },
  { "rooms": ["workspace/*"], "allow": ["publish"], "kind": "data", "label": "chat" },
  { "rooms": ["*"], "allow": ["discover", "subscribe"], "participant": "alice", "kind": "video" }
]
```

Actions are `join`, `publish`, `discover` and `subscribe`. Room patterns are exact
strings, `*`, or a prefix ending in `/*`; other wildcard positions are invalid.
Filters are exact participant ID, kind (`audio`, `video` or `data`) and label.
Omission means unrestricted; explicit null is invalid. Join cannot have filters,
and publish cannot filter by participant. An empty grant array authenticates a
principal but grants no access.

Each token permits at most 32 grants and 64 total room patterns, counted as
received, including duplicates. Each grant requires one to four distinct known
actions. Identifiers, labels and patterns are nonempty, control-free UTF-8 strings
of at most 256 bytes. No Unicode normalization or case folding is performed.
Unknown fields, malformed shapes, duplicate JSON members and incompatible filters
are rejected. The verifier parses only this fixed schema, not arbitrary JSON trees.

Admission requires an explicitly selected room ID matching `[A-Za-z0-9_-]{1,36}`
and a matching join grant. Room IDs preserve case.
The JWT never supplies a default room. Renewal and exact-resource cleanup first
bind the authenticated project and participant to the accepted resource. A valid
renewal can reduce grants, including removal of join, which terminates that resource.

## Wire representation

Exactly these UTF-8 JSON fields, in this order, with no whitespace:

```text
{"alg":"EdDSA","kid":"<canonical key ID>","typ":"pb+jwt"}
{"iss":"<canonical project ID>","aud":"pb","sub":"<participant>","exp":<unsigned decimal digits>,"jti":"<canonical UUIDv4>","grants":[<grant objects>]}
```

Unpadded base64url encode each object, join with `.`, sign those literal ASCII
bytes using Ed25519 and the imported private key, then append `.` and the unpadded
base64url 64-byte signature. Grant fields serialize in `rooms`, `allow`,
`participant`, `kind`, `label` order, omitting absent filters. No `iat`, `nbf`,
`room`, `role`, or default JWT headers are emitted. The verifier rejects unknown
header and claim members; an optional empty `crit` header is accepted. The server matches the token project ID and key ID against the public key registry,
verifies the signature, and requires `now < exp`. The `jti` must be lowercase,
hyphenated UUID text with version 4 and RFC variant bits. The server records it
with the verified project and connection only after admission or renewal succeeds.
A token can be used on multiple connections. Its ID does not enforce uniqueness,
prevent replay, or change permissions.

The complete encoded token is limited to 8,192 bytes, including both separators
and the signature. Signers reject oversized output before signing. The server
checks the received size before decoding or signature verification, including
credential renewal. This limit does not replace identifier validation.

## Fixture adaptation

`valid[].input.expiration` is a decimal string to avoid JSON numeric precision
loss. Adapters use bigint (TypeScript), uint64 (Go), int (Python), or u64 (Rust), not a
public string expiration API. Each accepted fixture records canonical IDs, seed/public
key bytes, raw JSON, signing input, signature, and complete token. All packages
consume every valid and every applicable invalid case for their opaque-key
signing API through public entry points. Component-format rejection fixtures
remain internal ID/codec cases; client SDKs don't reimplement those formats.
`input.requested_room` is verifier-test metadata, never a signer argument or JWT
claim. Null invalid values represent omitted inputs. `representation: number`
invalid expirations exercise an inexact native numeric type; `string` exercises
the forbidden string API input. Go rejects wrong types and omission of its
mandatory uint64 argument at compile time; its adapter establishes those cases
with the Go type checker. Rust uses compile-fail doctests against the packaged
crate for its corresponding typed boundary. Runtime adapters check grant structure, scope bounds and expiration representation;
typed APIs reject unrepresentable argument types. The server independently checks
all received claims and enforces registry matching and resource authority.

`cases.json` is the maintenance source for accepted inputs, fixed fixture `jti`
values, seed bytes and rejections. The Rust generator uses native Ed25519
directly, not an SDK signer. Packaged SDK tests check that fresh IDs match the
signed claim and verify freshly issued signatures independently.
Run `./bazel run //server/auth:generate_vectors` and review the `vectors.json` diff.
Golden data is committed and never regenerated during tests. Its seed/public key
pair comes from RFC 8032 section 7.1 test 2, not development credentials.
Rust owns the shared conformance and server-authorization tests under this
package. Independent strict Ed25519 verification of every frozen signature,
including expiration zero, is anchored to the RFC public key. The Go/Python
consumer tests independently verify signatures with their native crypto.
These checks prevent a self-consistent generator/signer from redefining auth.
Run `./bazel test //server/auth:fast` for the shared owning gate.
