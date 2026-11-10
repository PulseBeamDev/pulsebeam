"""Direct generated Python object/async lifecycle boundary, without an SFU."""

import asyncio
import importlib
from pathlib import Path
import shutil
import sys
import tempfile


async def witness():
    core = importlib.import_module("contract.pulsebeam_agent_core")
    native = importlib.import_module("contract.pulsebeam_agent_native")
    agent = await native.Agent.create(core.AgentConfig(
        endpoint="http://pulsebeam.test", token="token",
        retry=core.RetryPolicy(
            initial_delay_ms=100, maximum_delay_ms=1000, maximum_attempts=2,
        ),
        log_level=core.LogLevel.WARN,
    ))
    try:
        revision = agent.snapshot().desired_revision
        local = agent.data("control", core.TopicMode.ORDERED)
        participant = agent.participant("remote")
        remote = participant.data("control", core.TopicMode.ORDERED)
        local_video = agent.video("camera")
        same_video = agent.video("camera")
        local_audio = agent.audio("microphone")
        remote_video = participant.video("camera")
        remote_audio = participant.audio("microphone")
        assert participant.id() == "remote"
        assert agent.snapshot().desired_revision == revision, "lookup acquired demand"
        video_source = await agent.create_video_input(native.VideoSourceConfig(
            encodings=[native.Encoding(
                rid=None, scale_down_by=1, max_bitrate_bps=None,
                max_framerate=None, temporal_layers=1,
            )], dependency_descriptor=True,
        ))
        audio_source = await agent.create_audio_input()
        assert isinstance(video_source, native.VideoSource)
        assert isinstance(audio_source, native.AudioSource)
        assert agent.snapshot().desired_revision == revision, "source creation published"
        configuration = core.PublicationConfig(enabled=True)
        video_publication = await local_video.publish(video_source, configuration)
        try:
            await same_video.publish(video_source, configuration)
        except native.NativeError.Resource as error:
            assert error.reason.is_ALREADY_OWNED()
        else:
            raise AssertionError("equivalent lookup bypassed publication exclusivity")
        await video_publication.configure(core.PublicationConfig(enabled=False))
        await video_publication.set_source(None)
        await video_publication.set_source(video_source)
        pending_bound = asyncio.create_task(video_publication.ready())
        await video_publication.close()
        try:
            await pending_bound
        except native.NativeError.Closed:
            pass
        else:
            raise AssertionError("publication without a native binding became ready")
        assert not video_source.is_closed(), "publication close stopped producer"
        video_replacement = await same_video.publish(video_source, configuration)
        feedback = video_source.feedback()
        sibling_feedback = video_source.feedback()
        pending_feedback = asyncio.create_task(feedback.next())
        await feedback.close()
        await feedback.close()
        assert feedback.is_closed() and (await pending_feedback).is_CLOSED()
        assert not sibling_feedback.is_closed() and not video_source.is_closed()
        await sibling_feedback.close()
        await video_source.close()
        assert not video_replacement.is_closed(), "source stop closed publication ownership"
        await video_replacement.close()
        audio_publication = await local_audio.publish(audio_source, configuration)
        try:
            await audio_source.submit(native.InputFrame.PCM(frame=native.PcmFrame(
                sample_rate_hz=48000, channels=1, samples=[-32768, 0, 32767], timestamp_us=0,
            )))
        except native.NativeError.NotReady:
            pass
        else:
            raise AssertionError("unallocated input accepted a frame")
        video_subscription = await remote_video.subscribe(core.VideoObservation(
            visible=False, height=0, min_height=0, min_fps=0, priority=1,
            playout_delay=core.PlayoutDelay.ADAPTIVE(),
        ))
        audio_subscription = await remote_audio.subscribe(core.AudioSubscriptionConfig(
            playout_delay=core.PlayoutDelay.ADAPTIVE(),
        ))
        audio_selection = await agent.select_audio(core.AudioDemand(
            pinned=[], selected=[], automatic=False, playout_delays=[], selector_delays=[],
        ))
        await audio_selection.close()
        assert not audio_subscription.is_closed(), "selection close retired named audio demand"
        pending_media = asyncio.create_task(video_subscription.observe(native.ReceiveMode.DECODED))
        try:
            await local.send(bytes([0, 255]))
        except native.NativeError.Resource as error:
            assert error.reason.is_NOT_CONNECTED()
        else:
            raise AssertionError("disconnected data send was admitted")
        closed_cursor = await local.receive()
        room_cursor = await local.receive()
        participant_cursor = await remote.receive()
        pending_close = asyncio.create_task(closed_cursor.next())
        await closed_cursor.close()
        await closed_cursor.close()
        assert (await pending_close).is_CLOSED()
        assert closed_cursor.is_closed()
        assert not room_cursor.is_closed()
        assert not participant_cursor.is_closed()
        pending_room = asyncio.create_task(room_cursor.next())
        pending_participant = asyncio.create_task(participant_cursor.next())
        snapshots = agent.snapshots()
        events = agent.events()
        assert (await snapshots.next()).is_SNAPSHOT()
        retired_snapshots = agent.snapshots()
        retired_events = agent.events()
        assert (await retired_snapshots.next()).is_SNAPSHOT()
        pending_retired_snapshot = asyncio.create_task(retired_snapshots.next())
        await retired_snapshots.close()
        await retired_snapshots.close()
        await retired_events.close()
        await retired_events.close()
        assert retired_snapshots.is_closed() and retired_events.is_closed()
        assert (await pending_retired_snapshot).is_CLOSED()
        assert (await retired_events.next()).is_CLOSED()
        assert not snapshots.is_closed() and not events.is_closed() and not agent.is_closed()
        pending_snapshot = asyncio.create_task(snapshots.next())
        pending_event = asyncio.create_task(events.next())
        await agent.disconnect()
        assert not agent.is_closed(), "disconnect was terminal"
        assert not room_cursor.is_closed()
        assert not participant_cursor.is_closed()
        await agent.close()
        await agent.close()
        assert agent.is_closed()
        assert audio_source.is_closed() and audio_publication.is_closed()
        assert audio_subscription.is_closed() and video_subscription.is_closed()
        try:
            await pending_media
        except native.NativeError.Closed:
            pass
        else:
            raise AssertionError("terminal close did not end pending media acquisition")
        assert (await pending_snapshot).is_CLOSED()
        assert (await pending_event).is_CLOSED()
        assert (await pending_room).is_CLOSED()
        assert (await pending_participant).is_CLOSED()
        try:
            await agent.connect()
        except native.NativeError.Closed:
            pass
        else:
            raise AssertionError("closed Agent revived")
    finally:
        await agent.close()
    print("generated Python lifecycle contract PASS")


def main():
    bindings = Path(sys.argv[1]).resolve(strict=True)
    library = Path(sys.argv[2]).absolute()
    with tempfile.TemporaryDirectory(prefix="pulsebeam-python-contract-") as temporary:
        root = Path(temporary)
        package = root / "bindings" / "contract"
        package.mkdir(parents=True)
        for solib in library.parents[2].glob("_solib_*"):
            (root / solib.name).symlink_to(solib, target_is_directory=True)
        (package / "__init__.py").touch()
        for namespace in ("pulsebeam_agent_core", "pulsebeam_agent_native"):
            shutil.copyfile(bindings / f"{namespace}.py", package / f"{namespace}.py")
        (package / "libpulsebeam_agent_native.so").symlink_to(library)
        sys.path.insert(0, str(package.parent))
        asyncio.run(asyncio.wait_for(witness(), timeout=5))


if __name__ == "__main__":
    main()
