"""Generated Python/dynamic RTC boundary, using the Rust witness's real SFU."""

import asyncio
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
                topology=core.MediaTopology(
                    local_videos=1, local_audios=0, remote_videos=0, remote_audios=0
                ),
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
        await agent.replace_desired(
            core.DesiredState(
                connected=True,
                publications=[
                    core.PublicationIntent(slot="v0", label="foreign-camera", active=True)
                ],
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
        camera = await agent.local_video("v0")
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
        assert camera.is_closed(), "foreign source survived graph retirement"
        print("generated Python Agent.create/dynamic RTC/real SFU/native source/close PASS")
    finally:
        await asyncio.wait_for(agent.abort(), timeout=3)


def main():
    bindings = Path(os.environ["PULSEBEAM_NATIVE_BINDINGS"]).resolve(strict=True)
    library = Path(os.environ["PULSEBEAM_NATIVE_LIBRARY"]).resolve(strict=True)
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
