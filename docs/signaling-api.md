# Signaling API

Two representations share the same paths. `Content-Type` selects between them.

| Verb | Path | `application/sdp` | `application/json` |
|---|---|---|---|
| POST | `/api/v1/rooms/{room}/participants` | join (WHIP-shaped, unchanged) | join |
| PUT | `/api/v1/rooms/{room}/participants/{pid}` | 415 | resume / reconstruct |
| PATCH | `/api/v1/rooms/{room}/participants/{pid}` | re-offer (unchanged) | 415 |
| DELETE | `/api/v1/rooms/{room}/participants/{pid}` | leave (unchanged) | leave |

The SDP surface is byte-for-byte what it always was, including that it requires a `Content-Type`
but ignores its value, and that `DELETE` without a credential is unconditional. A golden test suite
asserts this.

**Renegotiation is not part of this API.** `PATCH` is delete-then-create internally, so adding or
removing a track mid-session is not supported by either representation.

## The Room

A Room is a real, ephemeral runtime resource: it groups active participants, tracks, data, routing
and media compatibility while activity exists. It is *not* a durable business entity and has no
independent lifecycle. The precise property is **ephemeral and non-durable** — not "stateless".

| Concept | Owner | Durability |
|---|---|---|
| Room lifecycle, membership, roles | your backend | durable |
| Authorization to join | your backend → JWT | short-lived capability |
| `room_id` | your chosen identity for the Room | ephemeral; no CRUD object |
| Participant presence | PulseBeam | ephemeral / reconcilable |
| Tracks, subscriptions, subrooms | PulseBeam | ephemeral |
| PeerConnection / BWE / RTP state | owning SFU | ephemeral, single-writer |

Consequences:

- **There are no room endpoints.** No create, update, delete, or list. The JWT's `pb.room` claim is
  how your backend grants a room namespace; it is the lifecycle boundary, not merely an ACL check.
- **Resume never requires the Room to exist.** If the last participant left and the room was swept,
  a `PUT` re-materializes it exactly as a fresh join would.
- **The SFU persists no identity.** `sub`, display name and capabilities are echoed from the token
  presented on each request and never stored beyond the live connection.

## Credentials

| Credential | Minted by | Proves | Sent on |
|---|---|---|---|
| Access JWT (`Authorization: Bearer`) | your backend | who you are, and what you may do in this room | POST, PUT, DELETE |
| `resume_token` | the SFU, on every POST/PUT | which `ParticipantId` is yours | PUT only |
| `connection_id` (`If-Match`) | the SFU, per connection | you hold the current live connection | DELETE |

`PUT` requires **both** a fresh JWT and the resume token, checked against each other:

- `jwt.sub` must equal the resume token's `sub` → else `403 subject_mismatch`
- `jwt.pb.room` must equal the token's room and the path → else `403 room_mismatch`
- the token's participant must equal the path → else `403 participant_mismatch`

The resume token is deliberately **not** clamped to the lifetime of the token that minted it.
Clamping would kill a long session exactly when resume matters most. Instead the client fetches a
current JWT from your backend and presents it alongside, so expiry, revocation and capability
changes are re-evaluated on every resume against your current answer.

`PUT` deliberately does **not** require `connection_id`: after a restart the client's copy names a
connection the server has forgotten. A stolen resume token alone is useless — an attacker also
needs a JWT minted for your `sub`.

## What resumption preserves

Media state can never survive a restart; a resume is always a fresh PeerConnection, on both sides.
What survives is the participant's **identity in the room**: the same `ParticipantId`, therefore the
same `TrackId`s. Subscribers see the publisher's tracks reappear rather than churn.

Track ids are derived from the participant id and the media line's **ordinal**, not its SDP mid. A
mid is minted fresh by the peer's SDP engine on every connection, so deriving from it would give a
resumed participant new tracks and quietly break every existing subscription. SDP fixes m-line order
across renegotiation, which makes the ordinal stable by construction.

A client resuming must therefore present its media lines in the same order it first did. The bundled
agent does this automatically: it stores a blueprint at join and rebuilds from it.

`PUT` is create-or-replace at a client-known URI: `201` when nothing was live and the participant
was genuinely rebuilt, `200` when a live one was replaced.

## Access token claims

Header must carry `kid`; `alg` is `EdDSA` or `ES256`. The algorithm is taken from the *configured
key*, never from the token, with a single-entry allowlist — this is the alg-confusion defence.

```jsonc
{
  "iss": "https://app.example.com",
  "sub": "user_1042",          // end-user identity; resume is bound to it
  "aud": "your-audience",      // must match a configured audience
  "exp": 1786294800,
  "iat": 1786291200,
  "jti": "unique-per-token",
  "pb": {
    "room": "standup",         // must equal the path segment exactly; no wildcards
    "name": "Ada",             // optional
    "publish": true,           // default true
    "subscribe": true,         // default true
    "max_duration_secs": 3600  // optional; bounds session_expires_at
  }
}
```

`iss`, `sub`, `aud`, `exp`, `jti` and `pb.room` are required. Clock skew tolerance defaults to 60s,
and a token whose `exp - iat` exceeds one hour is refused.

## Configuration

```
--jwt-key kid:ed25519:<base64 raw public key>   # or kid:es256:<base64 uncompressed point>
--jwt-audience your-audience                    # required; repeatable
--jwt-issuer https://app.example.com            # optional; repeatable
--resume-key kid:<base64 32 bytes>              # cluster-wide; repeatable, first signs
--require-auth                                  # also require a token on the SDP endpoints
```

All also available as `PULSEBEAM_JWT_KEY`, `PULSEBEAM_JWT_AUDIENCE`, `PULSEBEAM_JWT_ISSUER`,
`PULSEBEAM_RESUME_KEY`, `PULSEBEAM_REQUIRE_AUTH`.

Three refusals, because a node that starts with authentication silently disabled is the failure mode
worth preventing:

- a malformed key spec aborts startup rather than being skipped
- keys without an audience refuse to enable auth at all, since without an audience a token minted
  for another service would verify here
- an absent `--resume-key` warns loudly and generates a per-process random one, which cannot survive
  the restart that resumption exists to handle. Always set it in production, identically on every
  node.

With no `--jwt-key` at all, the JSON endpoints return `503 auth_not_configured` and the SDP
endpoints are unaffected — which is what an existing deployment gets by default.

## WHIP compatibility

A WHIP client sending `application/sdp` uses the original handlers unchanged. WHIP already permits
`Authorization: Bearer` (RFC 9725 §4.1), so JWT works for WHIP clients with no protocol change once
`--require-auth` is set.

Resumption is JSON-only: WHIP has nowhere to carry a resume token. A WHIP client whose node restarts
gets a 404 and re-POSTs as a new participant. The token is deliberately not smuggled into the
`Location` URL, which would put a bearer-equivalent credential into proxy and access logs.

## Errors

Every non-2xx JSON response is:

```json
{ "error": { "code": "room_mismatch", "message": "token is not valid for this room" } }
```

`code` is stable and machine-readable. `401` responses also carry `WWW-Authenticate`.

| Status | Codes |
|---|---|
| 400 | `invalid_json`, `invalid_sdp`, `invalid_id`, `bad_request` |
| 401 | `missing_token`, `malformed_token`, `unknown_kid`, `invalid_signature`, `token_expired`, `token_not_yet_valid`, `invalid_audience`, `invalid_issuer`, `invalid_resume_token`, `resume_token_expired`, `unknown_resume_kid` |
| 403 | `room_mismatch`, `subject_mismatch`, `participant_mismatch`, `publish_denied`, `subscribe_denied` |
| 404 | `participant_not_found` |
| 412 | `connection_mismatch` |
| 415 | `unsupported_media_type` |
| 429 | `rate_limited` (carries `retry_after_ms`) |
| 500 / 503 | `internal`, `service_unavailable`, `auth_not_configured` |

Clients should treat credential-shaped codes (`resume_token_expired`, the `*_mismatch` family,
`token_expired`) as terminal and stop retrying; the others are transient.

The live OpenAPI document is served at `/api-docs/openapi.json`, with Swagger UI at `/swagger-ui`.
