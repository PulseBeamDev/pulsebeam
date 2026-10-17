"""Publicly known API key embedded for local --dev servers only."""
import json
from importlib.resources import files

_fixture = json.loads(files(__package__).joinpath("development.json").read_text(encoding="utf-8"))

# Insecure, publicly known API key for local --dev only.
INSECURE_DEV_API_KEY = _fixture["apiKey"]
