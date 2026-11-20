"""Direct generated Python object/async lifecycle boundary, without an SFU."""

import asyncio
import importlib
import inspect
from pathlib import Path
import shutil
import sys
import tempfile


async def witness():
    core = importlib.import_module("contract.pulsebeam_agent_core")
    native = importlib.import_module("contract.pulsebeam_agent_native")
    for endpoint, token, maximum_attempts in (
        ("relative", "secret-token", 2),
        ("http://pulsebeam.test", "", 2),
        ("http://pulsebeam.test", "secret-token", 0),
    ):
        try:
            native.Agent(core.AgentConfig(
                endpoint=endpoint, token=token,
                retry=core.RetryPolicy(
                    initial_delay_ms=100, maximum_delay_ms=1000,
                    maximum_attempts=maximum_attempts,
                ),
                log_level=core.LogLevel.WARN,
            ))
        except native.NativeError.Operation as error:
            assert error.info.code == core.ErrorCode.INVALID_ARGUMENT
            assert error.info.message == "Invalid Agent configuration"
            assert "secret-token" not in error.info.message
        else:
            raise AssertionError("invalid Agent configuration was not rejected synchronously")
    session = core.AgentConfig(
        endpoint="http://pulsebeam.test", token="token",
        retry=core.RetryPolicy(
            initial_delay_ms=100, maximum_delay_ms=1000, maximum_attempts=2,
        ),
        log_level=core.LogLevel.WARN,
    )
    agent = native.Agent(session)
    media_agent = native.Agent.create_with_media(native.NativeAgentConfig(
        session=session, media=native.MediaConfig(
            audio_input=native.AudioInput.PCM(),
            video_input=native.VideoInput.I420,
            video_publications=[],
        ),
    ))
    assert not inspect.isawaitable(media_agent)
    assert media_agent.state().status == core.AgentStatus.IDLE
    assert media_agent.state().room is None
    assert media_agent.support_report().connection_id is None
    assert not inspect.isawaitable(agent)
    try:
        initial_state = agent.state()
        state_updates = agent.updates()
        assert isinstance(state_updates, native.AgentUpdates)
        assert initial_state.status == core.AgentStatus.IDLE
        assert initial_state.room is None and initial_state.desired_room is None
        assert isinstance(initial_state.observation_revision, int)
        assert (await state_updates.next()).observation_revision == initial_state.observation_revision
        snapshot = agent.snapshot()
        assert not hasattr(snapshot, "catalog") and not hasattr(snapshot, "mapping")
        for obsolete in ["CatalogSnapshot", "ParticipantInfo", "Publication", "RemoteTrack", "TrackSpec", "MappingSnapshot", "TrackMapping"]:
            assert not hasattr(core, obsolete), obsolete
        revision = snapshot.desired_revision
        assert agent.snapshot().join_revision == 0
        assert isinstance(agent.snapshot().join_revision, int)
        assert agent.snapshot().desired_room is None
        assert agent.snapshot().credential_revision == 0
        assert isinstance(agent.snapshot().credential_revision, int)
        assert agent.snapshot().accepted_credential_revision is None
        assert agent.snapshot().credential_outcome is None
        assert agent.renew("renewed-token") is None
        assert agent.snapshot().credential_revision == 1
        assert agent.snapshot().desired_room is None
        local = agent.data("control", core.TopicMode.ORDERED)
        room = agent.room("room")
        assert isinstance(room, native.RoomRef) and room.name() == "room"
        watch = room.watch()
        assert isinstance(watch, native.Watch) and not inspect.isawaitable(watch)
        watch_state = watch.state()
        assert watch_state.room == "room" and watch_state.limit == 100
        assert watch_state.status == core.WatchStatus.LOADING and watch_state.page is None
        watch_updates = watch.updates()
        assert isinstance(watch_updates, native.WatchUpdates)
        assert (await watch_updates.next()).observation_revision == watch_state.observation_revision
        assert watch.close() is None and watch.close() is None
        assert (await watch_updates.next()).status == core.WatchStatus.CLOSED
        assert await watch_updates.next() is None
        watch_updates.close()
        scoped_watch = room.watch(core.WatchConfig(limit=3, cursor=bytes([7, 8])))
        assert scoped_watch.state().limit == 3 and bytes(scoped_watch.state().cursor) == bytes([7, 8])
        scoped_watch.close()
        assert not hasattr(agent, "participant") and not hasattr(local, "receive")
        participant = room.participant("remote")
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
        video_publication = local_video.publish()
        assert not inspect.isawaitable(video_publication)
        assert video_publication.state().configuration.enabled is True
        pipe = video_source.pipe_to(video_publication)
        assert not inspect.isawaitable(pipe)
        assert isinstance(pipe, native.Pipe)
        pipe_updates = pipe.updates()
        initial_pipe = pipe.state()
        assert isinstance(pipe_updates, native.PipeUpdates)
        assert isinstance(initial_pipe.status, core.PipeStatus)
        assert (await pipe_updates.next()).observation_revision == initial_pipe.observation_revision
        publication_updates = video_publication.updates()
        initial_publication = video_publication.state()
        assert isinstance(publication_updates, native.PublicationUpdates)
        assert initial_publication.admission.status == core.PublicationAdmissionStatus.PENDING
        assert initial_publication.admission.demand == core.PublicationDemand.UNKNOWN
        assert initial_publication.configuration.enabled is True
        assert isinstance(initial_publication.configuration.desired_revision, int)
        assert (await publication_updates.next()).observation_revision == initial_publication.observation_revision
        try:
            same_video.publish()
        except native.NativeError.Resource as error:
            assert error.reason.is_ALREADY_OWNED()
        else:
            raise AssertionError("equivalent lookup bypassed publication exclusivity")
        await video_publication.configure(core.PublicationConfig(enabled=False))
        configured_publication = video_publication.state()
        assert configured_publication.admission.status == core.PublicationAdmissionStatus.PENDING
        assert configured_publication.admission.demand == core.PublicationDemand.UNKNOWN
        assert configured_publication.configuration.enabled is False
        assert configured_publication.configuration.desired_revision > initial_publication.configuration.desired_revision
        assert (await publication_updates.next()).configuration.enabled is False
        no_op_revision = configured_publication.configuration.desired_revision
        await video_publication.configure(core.PublicationConfig(enabled=False))
        assert video_publication.state().configuration.desired_revision == no_op_revision
        await video_publication.configure(core.PublicationConfig(enabled=True))
        assert video_publication.state().configuration.enabled is True
        await video_publication.configure(core.PublicationConfig(enabled=False))
        assert video_publication.state().configuration.enabled is False
        unrelated_data = local.publish()
        unrelated_data.close()
        assert unrelated_data.state().closed
        assert pipe.state().status != core.PipeStatus.CLOSED
        assert not video_publication.state().closed and video_publication.state().input_attached
        assert not video_source.is_closed(), "unrelated resource close stopped borrowed producer"
        await pipe.close()
        assert pipe.state().status == core.PipeStatus.CLOSED
        assert (await pipe_updates.next()).status == core.PipeStatus.CLOSED
        assert await pipe_updates.next() is None
        pipe_updates.close()
        assert not video_source.is_closed(), "pipe close stopped borrowed producer"
        pending_bound = asyncio.create_task(video_publication.ready())
        await video_publication.close()
        assert video_publication.state().closed
        assert (await publication_updates.next()).closed is True
        assert (await publication_updates.next()) is None
        publication_updates.close()
        try:
            await pending_bound
        except native.NativeError.Closed:
            pass
        else:
            raise AssertionError("publication without a native binding became ready")
        assert not video_source.is_closed(), "publication close stopped producer"
        video_replacement = same_video.publish(configuration)
        await video_replacement.set_source(video_source)
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
        audio_publication = local_audio.publish()
        assert not inspect.isawaitable(audio_publication)
        assert audio_publication.state().configuration.enabled is True
        await audio_publication.set_source(audio_source)
        audio_publication_updates = audio_publication.updates()
        initial_audio_publication = await audio_publication_updates.next()
        assert initial_audio_publication.admission.status == core.PublicationAdmissionStatus.PENDING
        pending_audio_publication = asyncio.create_task(audio_publication_updates.next())
        try:
            await audio_source.submit(native.InputFrame.PCM(frame=native.PcmFrame(
                sample_rate_hz=48000, channels=1, samples=[-32768, 0, 32767], timestamp_us=0,
            )))
        except native.NativeError.NotReady:
            pass
        else:
            raise AssertionError("unallocated input accepted a frame")
        closed_before_admission = remote_video.subscribe(core.VideoObservation(
            visible=False, height=0, min_height=0, min_fps=0, priority=1,
            playout_delay=core.PlayoutDelay.ADAPTIVE(),
        ))
        assert not inspect.isawaitable(closed_before_admission)
        await closed_before_admission.close()
        assert closed_before_admission.is_closed()
        explicit_audio = remote_audio.subscribe(core.AudioSubscriptionConfig(
            playout_delay=core.PlayoutDelay.ADAPTIVE(),
        ))
        await explicit_audio.close()
        video_subscription = remote_video.subscribe()
        assert not inspect.isawaitable(video_subscription)
        audio_subscription = remote_audio.subscribe()
        assert not inspect.isawaitable(audio_subscription)
        video_state = video_subscription.state()
        audio_state = audio_subscription.state()
        assert isinstance(video_state, core.VideoSubscriptionState)
        assert isinstance(audio_state, core.AudioSubscriptionState)
        assert video_state.configuration == core.VideoObservation(
            visible=True, height=0, min_height=0, min_fps=0, priority=0,
            playout_delay=core.PlayoutDelay.ADAPTIVE(),
        )
        assert audio_state.playout_delay.is_ADAPTIVE()
        video_updates = video_subscription.updates()
        audio_updates = audio_subscription.updates()
        assert isinstance(video_updates, native.VideoSubscriptionUpdates)
        assert isinstance(audio_updates, native.AudioSubscriptionUpdates)
        assert (await video_updates.next()).observation_revision == video_state.observation_revision
        assert (await audio_updates.next()).observation_revision == audio_state.observation_revision
        audio_selection = await agent.select_audio(core.AudioDemand(
            pinned=[], selected=[], playout_delays=[], selector_delays=[],
        ))
        await audio_selection.close()
        assert not audio_subscription.is_closed(), "selection close retired named audio demand"
        frame_readers = []
        pending_frames = []
        for subscription, reader_type in (
            (video_subscription, native.VideoFrames),
            (audio_subscription, native.AudioFrames),
        ):
            reader = subscription.frames()
            assert isinstance(reader, reader_type) and not inspect.isawaitable(reader)
            try:
                subscription.frames()
            except native.NativeError.Operation as error:
                assert error.info.code == core.ErrorCode.CONFLICT
            else:
                raise AssertionError("second active frame reader was admitted")
            reads = {asyncio.create_task(reader.next()), asyncio.create_task(reader.next())}
            done, pending = await asyncio.wait(reads, return_when=asyncio.FIRST_COMPLETED)
            assert len(done) == 1 and len(pending) == 1, "inert frame reads did not leave one pending"
            try:
                await done.pop()
            except native.NativeError.Operation as error:
                assert error.info.code == core.ErrorCode.CONFLICT
            else:
                raise AssertionError("concurrent frame read was admitted")
            assert reader.close() is None
            assert await pending.pop() is None
            assert not subscription.is_closed(), "reader close retired subscription"
            replacement = subscription.frames()
            assert isinstance(replacement, reader_type) and not inspect.isawaitable(replacement)
            frame_readers.append(replacement)
            pending_frames.append(asyncio.create_task(replacement.next()))
        pending_media = asyncio.create_task(video_subscription.observe(native.ReceiveMode.DECODED))
        assert not hasattr(local, "send"), "inert data address exposed send"
        assert not hasattr(remote, "receive"), "inert data address exposed receive"
        data_publication = local.publish()
        assert not inspect.isawaitable(data_publication)
        data_state = data_publication.state()
        assert isinstance(data_state, core.DataPublicationState)
        assert data_state.track_id == "control" and data_state.delivery == core.TopicMode.ORDERED
        data_updates = data_publication.updates()
        sibling_data_updates = data_publication.updates()
        assert isinstance(data_updates, native.DataPublicationUpdates)
        assert (await data_updates.next()).observation_revision == data_state.observation_revision
        assert (await sibling_data_updates.next()).observation_revision == data_state.observation_revision
        data_updates.close()
        try:
            agent.data("control", core.TopicMode.ORDERED).publish()
        except native.NativeError.Resource as error:
            assert error.reason.is_ALREADY_OWNED()
        else:
            raise AssertionError("duplicate data publication was admitted")
        try:
            await data_publication.send(bytes([0, 255]))
        except native.NativeError.Operation as error:
            assert error.info.code == core.ErrorCode.NOT_READY
        else:
            raise AssertionError("disconnected data send was admitted")
        data_publication.close()
        assert data_publication.state().closed
        assert (await sibling_data_updates.next()).closed
        assert await sibling_data_updates.next() is None
        sibling_data_updates.close()
        try:
            await data_publication.send(bytes([0, 255]))
        except native.NativeError.Operation as error:
            assert error.info.code == core.ErrorCode.CLOSED
        else:
            raise AssertionError("closed data publication accepted a send")
        replacement_data = local.publish()
        replacement_data.close()
        closed_cursor = remote.subscribe()
        independent_cursor = remote.subscribe()
        participant_cursor = remote.subscribe()
        assert not any(inspect.isawaitable(cursor) for cursor in (
            closed_cursor, independent_cursor, participant_cursor,
        ))
        receiver_state = closed_cursor.state()
        receiver_updates = closed_cursor.updates()
        assert isinstance(receiver_state, core.DataSubscriptionState)
        assert isinstance(receiver_updates, native.DataSubscriptionUpdates)
        assert (await receiver_updates.next()).observation_revision == receiver_state.observation_revision
        pending_close = asyncio.create_task(closed_cursor.next())
        await closed_cursor.close()
        await closed_cursor.close()
        assert (await pending_close).is_CLOSED()
        assert closed_cursor.is_closed()
        assert closed_cursor.state().closed and (await receiver_updates.next()).closed
        assert await receiver_updates.next() is None
        receiver_updates.close()
        assert not independent_cursor.is_closed()
        assert not participant_cursor.is_closed()
        pending_independent = asyncio.create_task(independent_cursor.next())
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
        assert not independent_cursor.is_closed()
        assert not participant_cursor.is_closed()
        await agent.close()
        await agent.close()
        assert agent.is_closed()
        assert audio_source.is_closed() and audio_publication.is_closed()
        assert audio_subscription.is_closed() and video_subscription.is_closed()
        for reader, pending in zip(frame_readers, pending_frames):
            assert await pending is None
            assert await reader.next() is None
            assert reader.close() is None
        try:
            await pending_media
        except native.NativeError.Closed:
            pass
        else:
            raise AssertionError("terminal close did not end pending media acquisition")
        assert (await pending_snapshot).is_CLOSED()
        assert (await pending_event).is_CLOSED()
        assert (await pending_independent).is_CLOSED()
        assert (await pending_participant).is_CLOSED()
        try:
            agent.join("room")
        except native.NativeError.Operation as error:
            assert error.info.code == core.ErrorCode.CLOSED
        else:
            raise AssertionError("closed Agent revived")
        assert video_subscription.state().closed and (await video_updates.next()).closed
        assert audio_subscription.state().closed and (await audio_updates.next()).closed
        assert await video_updates.next() is None and await audio_updates.next() is None
        video_updates.close()
        audio_updates.close()
        final_audio_publication = await pending_audio_publication
        assert final_audio_publication.closed and audio_publication.state().closed
        assert await audio_publication_updates.next() is None
        audio_publication_updates.close()
        final_state = await state_updates.next()
        assert final_state.status == core.AgentStatus.CLOSED
        assert final_state.desired_credential_revision == agent.snapshot().credential_revision
        assert initial_state.status == core.AgentStatus.IDLE
        assert (await state_updates.next()) is None
        assert agent.state().observation_revision == final_state.observation_revision
        state_updates.close()
        state_updates.close()
    finally:
        await asyncio.gather(agent.close(), media_agent.close())
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
