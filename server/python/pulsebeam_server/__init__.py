"""Native server-side PulseBeam SDK. Never expose signing secrets to clients."""

from ._auth import sign_participant_token

__all__ = ["sign_participant_token"]
