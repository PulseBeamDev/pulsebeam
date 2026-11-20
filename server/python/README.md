# pulsebeam-server

Native Python 3.10+ server SDK using `cryptography`'s Ed25519 implementation.
Ordinary crypto native extensions are supported; there is no PulseBeam network
call, Rust signer, WASM, or language bridge. Public key-registry publication is handled separately by the PulseBeam server.

## Local development

`from pulsebeam_server import INSECURE_DEV_API_KEY` provides one publicly known
API key for local `--dev` servers. Pass it to
`sign_participant_token(api_key, grants=..., participant=..., expiration=...)`.
**Never use these credentials in production or expose a development server.**

## Local installation

```sh
./bazel build //server/python:package
# In your application's virtual environment:
python -m pip install bazel-bin/server/python/dist/pulsebeam_server-0.1.0-py3-none-any.whl
```

The distribution is also buildable with standard Python build tooling from
`server/python/pyproject.toml`. Import only the public package:

```python
from pulsebeam_server import sign_participant_token

def issue_token(api_key: str) -> str:
    issued = sign_participant_token(
        api_key,
        grants=[{"rooms": ["general"], "allow": ["join", "publish", "discover", "subscribe"]}],
        participant="Alice",
        expiration=2000000000,
    )
    return issued.token
```

The signer returns `SignedParticipantToken` with `token` and its signed UUIDv4
`jti`. Send only the token to the client. Retain `jti` on your backend if you need
incident correlation; do not log the JWT.

The API takes one opaque API key plus grants, participant and expiration.
The SDK never reads the environment. Expiration must be an exact int in `0..18446744073709551615`,
representing absolute Unix seconds, not a TTL. Booleans, floats and strings are
not accepted. Invalid values raise without returning a token. No clock is
consulted: zero and past timestamps can be signed, but the server rejects tokens
at and after their expiration.

Participant IDs match `[A-Za-z0-9_-]{1,36}` and preserve case. Grant room
patterns and labels are nonempty UTF-8 strings of at most 256 bytes,
without control characters. The SDK validates these bounds and the
[grant schema](../auth/README.md#grants), preserving exact text and case.
The exported `Grant` TypedDict describes the input. Omit unrestricted filters;
explicit None is invalid. Empty grants grant no access. The SFU validates issuer
and key formats and registry membership. Treat the API key as one opaque secret.

Keep secrets on servers only, never in clients or logs. Offline signing cannot
check registration or whether a secret belongs to the supplied project/key.
The server performs these checks. SDK error messages do not contain credentials
or decoded seeds. There is no public verification or credential-generation API.

See [shared conformance](../auth/README.md). Repository validation:
`./bazel test //server/python:fast`.
