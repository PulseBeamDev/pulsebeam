"""Generated Python/dynamic RTC boundary, using the Rust witness's real SFU."""

import asyncio
import ctypes
import importlib
import os
from pathlib import Path
import shutil
import sys
import tempfile


async def witness(endpoint, token, package):
    core = importlib.import_module(f"{package}.pulsebeam_agent_core")
    native = importlib.import_module(f"{package}.pulsebeam_agent_native")
    agent = await native.Agent.create(
        native.NativeAgentConfig(
            session=core.AgentConfig(
                endpoint=endpoint,
                token=token,
                retry=core.RetryPolicy(
                    initial_delay_ms=100, maximum_delay_ms=1000, maximum_attempts=2
                ),
                log_level=core.LogLevel.WARN,
            ),
            media=native.MediaConfig(
                audio_input=native.AudioInput.PCM(),
                video_input=native.VideoInput.I420,
                video_publications=[],
            ),
        )
    )
    try:
        updates = agent.snapshots()
        publication = await agent.publication(core.TrackSpec.VIDEO(label="foreign-camera"))
        camera = await publication.input()
        await agent.replace_desired(
            core.DesiredState(
                connected=True,
                publications=[],
                video=[],
                audio=core.AudioDemand(
                    pinned=[], selected=[], automatic=False,
                    playout_delays=[], selector_delays=[]
                ),
                topic_publishers=[],
                topic_subscribers=[],
            )
        )
        while True:
            update = await updates.next()
            assert update.is_SNAPSHOT(), f"lost foreign snapshot observation: {update}"
            snapshot = update.snapshot
            assert not snapshot.connection.is_TERMINAL_FAILURE(), snapshot.failure
            if snapshot.connection.is_CONNECTED() and snapshot.participant_id:
                break
        assert snapshot.generation is not None
        await camera.ready()
        interest = await agent.audio_subscription(
            core.TrackSelector(participant_id="absent-publisher", label="microphone")
        )
        video_preferences = core.VideoObservation(
            visible=True, height=720, min_height=0, min_fps=0, priority=1,
            playout_delay=core.PlayoutDelay.ADAPTIVE(),
        )
        video_interest = await agent.video_subscription(
            core.TrackSelector(participant_id="absent-publisher", label="camera"),
            video_preferences,
        )
        assert not video_interest.is_closed(), "new foreign video interest is closed"
        video_preferences.visible = False
        await video_interest.configure_video(video_preferences)
        await video_interest.close()
        assert video_interest.is_closed(), "foreign video interest did not close"
        assert not interest.is_closed(), "video close ended the independent audio interest"
        audio_policy = core.AudioDemand(
            pinned=[], selected=[], automatic=True,
            playout_delays=[], selector_delays=[],
        )
        all_audio = await agent.audio_policy(audio_policy)
        audio_policy.automatic = False
        await all_audio.configure_audio_policy(audio_policy)
        try:
            await all_audio.observe_source("absent-source", native.ReceiveMode.DECODED)
        except native.NativeError.Closed:
            pass
        else:
            raise AssertionError("inactive audio policy authorized an exact-source observation")
        assert not all_audio.is_closed(), "refused observation ended its audio policy"
        await all_audio.close()
        assert all_audio.is_closed(), "foreign audio policy did not close"
        assert not interest.is_closed(), "policy close ended the independent named audio interest"
        await interest.close()
        assert interest.is_closed(), "foreign audio interest did not close"
        try:
            await interest.observe(native.ReceiveMode.DECODED)
        except native.NativeError.Closed:
            pass
        else:
            raise AssertionError("closed foreign audio interest admitted an observation")
        first = await camera.capture_time_us()
        second = await camera.capture_time_us()
        assert 0 < first <= second, "invalid native capture clock"
        await camera.submit(
            native.InputFrame.I420(
                frame=native.I420Frame(
                    width=160, height=96, timestamp_us=second,
                    data=bytes([180]) * (160 * 96) + bytes([90]) * (80 * 48)
                    + bytes([180]) * (80 * 48),
                    rotation_degrees=0,
                )
            )
        )
        # Submission proves ABI/native admission, not encode or remote delivery.
        await asyncio.wait_for(agent.close(), timeout=5)
        assert agent.snapshot().connection.is_DISCONNECTED()
        assert agent.statistics() is None, "foreign native graph not retired"
        assert not camera.is_closed(), "graph retirement ended the logical input"
        try:
            await camera.capture_time_us()
        except native.NativeError.NotReady:
            pass
        else:
            raise AssertionError("retired native target still admitted clock reads")
        await publication.close()
        assert publication.is_closed() and camera.is_closed(), "foreign owner not retired"
        print("generated Python Agent.create/dynamic RTC/real SFU/publication input/named media close PASS")
    finally:
        await asyncio.wait_for(agent.abort(), timeout=3)


def main():
    bindings = Path(os.environ["PULSEBEAM_NATIVE_BINDINGS"]).resolve(strict=True)
    library = Path(os.environ["PULSEBEAM_NATIVE_LIBRARY"]).resolve(strict=True)
    # Keep ELF $ORIGIN anchored to the declared runtime closure before loading the package alias.
    _native_library = ctypes.CDLL(str(library))
    with tempfile.TemporaryDirectory(prefix="native-foreign-") as temporary:
        package = "native_boundary"
        directory = Path(temporary) / package
        directory.mkdir()
        (directory / "__init__.py").touch()
        for module in ("pulsebeam_agent_core.py", "pulsebeam_agent_native.py"):
            shutil.copyfile(bindings / module, directory / module)
        (directory / "libpulsebeam_agent_native.so").symlink_to(library)
        sys.path.insert(0, temporary)
        asyncio.run(asyncio.wait_for(witness(sys.argv[1], sys.argv[2], package), timeout=15))


if __name__ == "__main__":
    main()
