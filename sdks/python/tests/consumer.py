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
from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat

artifacts = pathlib.Path(sys.argv[1])
vectors = json.loads(pathlib.Path(sys.argv[2]).read_text())
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
    assert (site / "pulsebeam_server/py.typed").exists()
    for v in vectors["valid"]:
        args = {**v["input"], "expiration": int(v["input"]["expiration"])}
        token = sign(**args)
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
        args = {**vectors["base"], "expiration": int(vectors["base"]["expiration"])}
        if v["value"] is None:
            del args[v["field"]]
        elif v.get("representation") == "number":
            args[v["field"]] = float(v["value"])
        else:
            args[v["field"]] = v["value"]
        try:
            sign(**args)
        except (TypeError, ValueError) as error:
            diagnostic = str(error) + repr(error)
            if isinstance(args.get("secret"), str) and args["secret"]:
                assert args["secret"] not in diagnostic, v["name"]
            assert "4ccd089b28ff96da" not in diagnostic
        else:
            raise AssertionError(v["name"])
    assert vectors["base"]["secret"] not in repr(sign)
