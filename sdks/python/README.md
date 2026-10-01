# pulsebeam-server

Native Python 3.10+ server SDK using `cryptography`'s Ed25519 implementation.
Ordinary crypto native extensions are supported; there is no PulseBeam network
call, Rust signer, WASM, or language bridge. Public registry publication is deferred.

## Local installation

```sh
./bazel build //sdks/python:package
# In your application's virtual environment:
python -m pip install bazel-bin/sdks/python/dist/pulsebeam_server-0.1.0-py3-none-any.whl
```

The distribution is also buildable with standard Python build tooling from
`sdks/python/pyproject.toml`. Import only the public package:

```python
import os
from pulsebeam_server import sign_participant_token

token = sign_participant_token(
    project_id=os.environ["PULSEBEAM_PROJECT_ID"],
    key_id=os.environ["PULSEBEAM_KEY_ID"],
    secret=os.environ["PULSEBEAM_SECRET"],
    room="general",
    participant="Alice",
    expiration=2000000000,
)
```

All six keyword-only arguments are mandatory. The SDK never reads the
environment. Expiration must be an exact int in `0..18446744073709551615`,
representing absolute Unix seconds, not a TTL. Booleans, floats and strings are
not accepted. Invalid values raise without returning a token. No clock is
consulted: zero and past timestamps can be signed, but the server rejects tokens
at and after their expiration.

Room and participant are case-sensitive external IDs of 1–36 ASCII characters
in `[A-Za-z0-9_-]`. Project/key IDs must be core V0 UUID v7 identities. The secret
is an `sk_0...` 32-byte Ed25519 seed, not a public or expanded private key.
Credential aliases are accepted and IDs canonicalized; external IDs are never
trimmed or normalized.

Keep secrets on servers only, never in clients or logs. Offline signing cannot
check registration or whether a secret belongs to the supplied project/key.
The server performs these checks. SDK error messages do not contain credentials
or decoded seeds. There is no public verification or credential-generation API.

See [shared conformance](../auth/README.md). Repository validation:
`./bazel test //sdks/python:fast`.
