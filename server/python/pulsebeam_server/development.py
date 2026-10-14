"""Insecure public credentials embedded for local --dev servers only."""
import json
from importlib.resources import files

_fixture = json.loads(files(__package__).joinpath("development.json").read_text(encoding="utf-8"))
INSECURE_DEV_CREDENTIALS = {
    "project_id": _fixture["projectId"],
    "key_id": _fixture["keyId"],
    "secret": _fixture["secret"],
}

# Insecure, publicly known fixture for local --dev only.
INSECURE_DEV_API_KEY = _fixture["apiKey"]
