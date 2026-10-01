# Participant-token conformance profile, version 1

This language-neutral profile and [vectors.json](vectors.json) are the SDK
correctness authority. The existing Rust server remains the authorization
boundary. No SDK or vector generator can alter server acceptance rules.

The shared Rust [`pulsebeam-auth`](../../crates/pulsebeam-auth/README.md) library
owns transport-free ID/key codecs. Core and the Rust SDK depend on it separately:
the SDK owns JWT generation, while core owns registry matching and verification.
This conformance package tests their boundary; it is not the shared library or a
public signing package.

## Inputs and codecs

All six inputs are mandatory: project ID, API key ID, signing secret, room
external ID, participant external ID, and absolute expiration in Unix seconds.
Expiration is an exact unsigned integer in `0..18446744073709551615`. No clock,
TTL, coercion, normalization of external IDs, or service lookup is involved.
Zero and past expiration can be signed, but authorize nothing at or after expiry.

Room and participant IDs are 1–36 ASCII characters in `[A-Za-z0-9_-]`. Preserve
case. Project and key IDs have exact case-sensitive `p_` and `kid_` prefixes,
followed by V0 marker (`0`, `O`, or `o`) and 26 Crockford Base32 characters. Encode
UUID bytes in big-endian bit order, with two zero **low** padding bits. Require
UUID v7 (high nibble of byte 6 equals 7) and RFC4122 variant (byte 8 high bits 10).
Emit canonical uppercase payload and `0` marker.

A signing secret has exact `sk_` prefix, V0 marker, and 52 Crockford digits. Its
32-byte Ed25519 seed is encoded big-endian with four zero **high** padding bits
(the first digit is 0 or 1). This differs from UUID padding. Payloads accept
lowercase, O/o = 0 and I/i/L/l = 1. U/u, whitespace, incorrect lengths, versions,
prefixes and padding are invalid. Public keys and expanded 64-byte private keys
are not signing secrets. Crockford alphabet: `0123456789ABCDEFGHJKMNPQRSTVWXYZ`.

## Wire representation

Exactly these UTF-8 JSON fields, in this order, with no whitespace:

```text
{"alg":"EdDSA","kid":"<canonical key ID>","typ":"pb+jwt"}
{"iss":"<canonical project ID>","aud":"pb","sub":"<participant>","room":"<room>","exp":<unsigned decimal digits>}
```

Unpadded base64url encode each object, join with `.`, sign those literal ASCII
bytes using Ed25519 and the decoded seed, then append `.` and the unpadded
base64url 64-byte signature. No `iat`, `nbf`, additional grants, or default JWT
headers are emitted. The server matches project/key/public key registration,
verifies the signature, and requires `now < exp`.

## Fixture adaptation

`valid[].input.expiration` is a decimal string to avoid JSON numeric precision
loss. Adapters use bigint (TypeScript), uint64 (Go), int (Python), or u64 (Rust), not a
public string expiration API. Each accepted fixture records canonical IDs, seed/public
key bytes, raw JSON, signing input, signature, and complete token. All packages
consume every valid and every applicable invalid case through public entry
points. Null invalid values represent omitted inputs. `representation: number`
invalid expirations exercise an inexact native numeric type; `string` exercises
the forbidden string API input. Go rejects wrong types and omission of its
mandatory uint64 argument at compile time; its adapter establishes those cases
with the Go type checker. Rust uses compile-fail doctests against the packaged
crate for its corresponding typed boundary. Runtime adapters classify every
invalid fixture and reject unknown fields/representations. No applicable
fixture is silently skipped.

`cases.json` is the maintenance source for accepted inputs, seed bytes and
rejections. The Rust generator uses native Ed25519 directly, not an SDK signer.
Run `./bazel run //sdks/auth:generate_vectors` and review the `vectors.json` diff.
Golden data is committed and never regenerated during tests. Its seed/public key
pair comes from RFC 8032 section 7.1 test 2, not development credentials.
Rust owns the shared conformance and server-authorization tests under this
package. Independent strict Ed25519 verification of every frozen signature,
including expiration zero, is anchored to the RFC public key. The Go/Python
consumer tests independently verify signatures with their native crypto.
These checks prevent a self-consistent generator/signer from redefining auth.
Run `./bazel test //sdks/auth:fast` for the shared owning gate.
