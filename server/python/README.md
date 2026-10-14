# pulsebeam-server

Native Python 3.10+ server SDK using `cryptography`'s Ed25519 implementation.
Ordinary crypto native extensions are supported; there is no PulseBeam network
call, Rust signer, WASM, or language bridge. Public key-registry publication is handled separately by the PulseBeam server.

## Local development

`from pulsebeam_server import INSECURE_DEV_API_KEY` provides one publicly known
API key for local `--dev` servers. Pass it to
`sign_participant_token_with_api_key(api_key, room=..., participant=..., expiration=...)`.
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
from pulsebeam_server import sign_participant_token_with_api_key

def issue_token(api_key: str) -> str:
    return sign_participant_token_with_api_key(
        api_key,
        room="general",
        participant="Alice",
        expiration=2000000000,
    )
```

The new API takes one explicit API key plus room, participant and expiration;
the legacy `sign_participant_token` accepts the original three-part credentials. The SDK never reads the
environment. Expiration must be an exact int in `0..18446744073709551615`,
representing absolute Unix seconds, not a TTL. Booleans, floats and strings are
not accepted. Invalid values raise without returning a token. No clock is
consulted: zero and past timestamps can be signed, but the server rejects tokens
at and after their expiration.

Room and participant are case-sensitive external IDs of 1–36 ASCII characters
in `[A-Za-z0-9_-]`. Treat the API key as an opaque server-side secret and pass it explicitly;
the SDK validates it but does not decide how your application stores it. External IDs are never trimmed or normalized.

Keep secrets on servers only, never in clients or logs. Offline signing cannot
check registration or whether a secret belongs to the supplied project/key.
The server performs these checks. SDK error messages do not contain credentials
or decoded seeds. There is no public verification or credential-generation API.

See [shared conformance](../auth/README.md). Repository validation:
`./bazel test //server/python:fast`.
