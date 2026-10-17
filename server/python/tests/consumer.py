"""Test the installed distribution, not a private source-path import."""
import base64
import importlib
import json
import pathlib
import sys
import tempfile

from installer import install
from installer.destinations import SchemeDictionaryDestination
from installer.sources import WheelFile
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey, Ed25519PublicKey
from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat, load_der_public_key

artifacts = pathlib.Path(sys.argv[1])
vectors = json.loads(pathlib.Path(sys.argv[2]).read_text())
development = json.loads(pathlib.Path(sys.argv[3]).read_text())
with tempfile.TemporaryDirectory() as directory:
    site = pathlib.Path(directory) / "site"
    destination = SchemeDictionaryDestination(
        {key: str(site) for key in ("purelib", "platlib", "headers", "scripts", "data")},
        interpreter=sys.executable,
        script_kind="posix",
    )
    wheels = list(artifacts.glob("*.whl"))
    assert len(wheels) == 1
    with WheelFile.open(wheels[0]) as wheel:
        install(wheel, destination, additional_metadata={"INSTALLER": b"pulsebeam-conformance"})
    sys.path.insert(0, str(site))
    sdk = importlib.import_module("pulsebeam_server")
    assert pathlib.Path(sdk.__file__).is_relative_to(site)
    sign = sdk.sign_participant_token
    assert sdk.INSECURE_DEV_API_KEY == development["apiKey"]
    assert json.loads((site / "pulsebeam_server/development.json").read_text()) == development
    assert (site / "pulsebeam_server/py.typed").exists()

    fixture_key = "p_0067MYZ1001R010000000000064.kid_0067MYZ1005R030010000000068.MC4CAQAwBQYDK2VwBCIEIEzNCJso_5banbbDRuwRTg9bijGfNaumJNqM9u1PuKb7"
    def adapt(i):
        return {"room": i["room"], "participant": i["participant"], "expiration": int(i["expiration"])}

    development_token = sign(
        sdk.INSECURE_DEV_API_KEY, room="demo", participant="alice", expiration=2000000000
    )
    assert json.loads(base64.urlsafe_b64decode(
        development_token.split(".")[1] + "=" * (-len(development_token.split(".")[1]) % 4)
    ))["iss"] == development["projectId"]

    public_der = base64.urlsafe_b64decode(
        development["publicKey"] + "=" * (-len(development["publicKey"]) % 4)
    )
    public_key = load_der_public_key(public_der)
    assert isinstance(public_key, Ed25519PublicKey)
    dev_parts = development_token.split(".")
    dev_sig = base64.urlsafe_b64decode(dev_parts[2] + "=" * (-len(dev_parts[2]) % 4))
    public_key.verify(dev_sig, (dev_parts[0] + "." + dev_parts[1]).encode())

    for v in vectors["valid"]:
        token = sign(fixture_key, **adapt(v["input"]))
        assert token == v["token"], v["name"]
        header, claims, signature = token.split(".")
        decode = lambda value: base64.urlsafe_b64decode(value + "=" * (-len(value) % 4))
        assert decode(header).decode() == v["header"]
        assert decode(claims).decode() == v["claims"]
        assert header + "." + claims == v["signing_input"]
        assert signature == v["signature"]
        private = Ed25519PrivateKey.from_private_bytes(bytes.fromhex(v["seed_hex"]))
        assert private.public_key().public_bytes(Encoding.Raw, PublicFormat.Raw).hex() == v["public_key_hex"]
        Ed25519PublicKey.from_public_bytes(bytes.fromhex(v["public_key_hex"])).verify(decode(signature), v["signing_input"].encode())

    for v in vectors["invalid"]:
        # Only expiration needs local checking; the SFU validates identities.
        if v["field"] != "expiration":
            continue
        args = adapt(vectors["base"])
        representation = v.get("representation")
        assert representation in (None, "number", "string"), v["name"]
        if v["value"] is None:
            del args[v["field"]]
        elif representation == "number":
            args[v["field"]] = float(v["value"])
        else:
            args[v["field"]] = v["value"]
        try:
            sign(fixture_key, **args)
        except (TypeError, ValueError) as error:
            diagnostic = str(error) + repr(error)
            assert fixture_key not in diagnostic, v["name"]
            assert "4ccd089b28ff96da" not in diagnostic
        else:
            raise AssertionError(v["name"])
        if representation == "number":
            try:
                exact = int(v["value"])
            except ValueError:
                continue
            if exact < 0 or exact > (1 << 64) - 1:
                try:
                    sign(fixture_key, **{**args, "expiration": exact})
                except ValueError:
                    pass
                else:
                    raise AssertionError(v["name"] + "-int")

    opaque = sdk.INSECURE_DEV_API_KEY.split(".")
    opaque[0], opaque[1] = "unknown-issuer", "unknown-key"
    signed = sign(
        ".".join(opaque), room="room with spaces",
        participant="participant/with/slashes", expiration=2000
    )
    head, claims = (
        json.loads(base64.urlsafe_b64decode(part + "=" * (-len(part) % 4)))
        for part in signed.split(".")[:2]
    )
    assert head["kid"] == opaque[1]
    assert claims["iss"] == opaque[0]
    assert claims["room"] == "room with spaces"
    assert claims["sub"] == "participant/with/slashes"

    for invalid in (
        sdk.INSECURE_DEV_API_KEY.replace(".MC4CAQ", ".invalid", 1),
        sdk.INSECURE_DEV_API_KEY[:-1],
        sdk.INSECURE_DEV_API_KEY + ".extra",
        sdk.INSECURE_DEV_API_KEY.replace(".", "..", 1),
    ):
        try:
            sign(invalid, room="demo", participant="alice", expiration=2000000000)
        except ValueError:
            pass
        else:
            raise AssertionError("invalid API key accepted")
