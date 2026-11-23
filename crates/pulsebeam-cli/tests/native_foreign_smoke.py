"""Generated Python/dynamic RTC boundary, using the Rust witness's real SFU."""

import asyncio
import importlib
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


async def witness(endpoint, token, reader_token, catalog_token, room, package):
    core = importlib.import_module(f"{package}.pulsebeam_agent_core")
    native = importlib.import_module(f"{package}.pulsebeam_agent_native")
    agent = native.Agent(
        core.AgentConfig(
            endpoint=endpoint,
            token=token,
            retry=core.RetryPolicy(
                initial_delay_ms=100, maximum_delay_ms=1000, maximum_attempts=2
            ),
            log_level=core.LogLevel.WARN,
        )
    )
    reader = native.Agent(core.AgentConfig(
        endpoint=endpoint, token=reader_token,
        retry=core.RetryPolicy(
            initial_delay_ms=100, maximum_delay_ms=1000, maximum_attempts=2,
        ),
        log_level=core.LogLevel.WARN,
    ))
    catalog_agent = native.Agent(core.AgentConfig(
        endpoint=endpoint, token=catalog_token,
        retry=core.RetryPolicy(
            initial_delay_ms=100, maximum_delay_ms=1000, maximum_attempts=2,
        ),
        log_level=core.LogLevel.WARN,
    ))
    watch = catalog_agent.room(room).watch()
    watch_updates = watch.updates()
    assert agent.state().status == core.AgentStatus.IDLE
    assert agent.state().room is None and agent.state().desired_room is None
    assert reader.state().status == core.AgentStatus.IDLE
    assert reader.state().room is None and reader.state().desired_room is None
    try:
        initial = await watch_updates.next()
        assert initial.status == core.WatchStatus.LOADING and initial.page is None

        async def catalog_page(predicate):
            async def matching_page():
                while True:
                    state = await watch_updates.next()
                    assert state is not None and state.error is None, state
                    if state.page is not None and predicate(state.page.participants()):
                        assert state.status == core.WatchStatus.LIVE
                        return state.page
            return await asyncio.wait_for(matching_page(), timeout=5)

        catalog_agent.join(room)
        first_page = await catalog_page(lambda participants: any(
            participant.id == "publisher" and any(
                publication.track_id == "production-camera" and publication.kind.is_VIDEO()
                for publication in participant.publications
            ) for participant in participants
        ) and any(
            participant.id == "foreign-catalog" and not participant.publications
            for participant in participants
        ))
        assert not first_page.has_more() and not first_page.next_cursor()
        assert all(participant.id != "foreign" for participant in first_page.participants())
        async def joined(owner):
            updates = owner.updates()
            try:
                while True:
                    state = await updates.next()
                    assert state is not None, "Agent closed before admission"
                    assert state.status != core.AgentStatus.FAILED, state.error
                    if state.status == core.AgentStatus.JOINED:
                        assert state.room == room
                        return
            finally:
                updates.close()
        data = agent.data("foreign-bytes", core.TopicMode.ORDERED).publish()
        room_cursor = reader.room(room).participant("foreign").data(
            "foreign-bytes", core.TopicMode.ORDERED,
        ).subscribe()
        participant_cursor = reader.room(room).participant("foreign").data(
            "foreign-bytes", core.TopicMode.ORDERED,
        ).subscribe()
        reader.join(room)
        camera = await agent.create_video_input(native.VideoSourceConfig(
            encodings=[native.Encoding(
                rid=None, scale_down_by=1, max_bitrate_bps=None,
                max_framerate=None, temporal_layers=1,
            )], dependency_descriptor=True,
        ))
        publication = agent.video("foreign-camera").publish()
        await publication.set_source(camera)
        agent.join(room)
        await asyncio.gather(joined(agent), joined(reader))
        changed_page = await catalog_page(lambda participants: any(
            participant.id == "foreign" and
            {publication.track_id for publication in participant.publications} == {"foreign-camera", "foreign-bytes"}
            for participant in participants
        ))
        assert changed_page.revision() > first_page.revision()
        assert all(participant.id != "foreign" for participant in first_page.participants())
        watch.close()
        final_watch = await watch_updates.next()
        assert final_watch.status == core.WatchStatus.CLOSED and final_watch.page is None
        assert await watch_updates.next() is None
        watch_updates.close()
        # The independent data subscriptions below must keep working after watch close.
        setup = bytes([0, 255, 128, 0])

        async def message(cursor, expected, participant_id="foreign"):
            while True:
                update = await cursor.next()
                assert update.is_EVENT() and update.event.is_MESSAGE(), update
                event = update.event
                assert event.participant_id == participant_id and event.publication_id
                if event.data == expected:
                    return event
                assert event.data == setup, "unexpected ordered data"

        async def establish_live_cursor():
            while True:
                await data.send(setup)
                await asyncio.sleep(0.2)

        # Ordered receive starts at a live frontier, not retained history.
        producer = asyncio.create_task(establish_live_cursor())
        try:
            await asyncio.wait_for(asyncio.gather(
                message(room_cursor, setup), message(participant_cursor, setup),
            ), timeout=5)
        finally:
            if producer.done():
                await producer
            else:
                producer.cancel()
                await asyncio.gather(producer, return_exceptions=True)
        payload = bytes([0, 255, 128, 1])
        await data.send(payload)
        room_message, participant_message = await asyncio.gather(
            message(room_cursor, payload), message(participant_cursor, payload),
        )
        assert room_message.data == payload == participant_message.data
        assert room_message.sequence == participant_message.sequence
        await room_cursor.close()
        assert (await room_cursor.next()).is_CLOSED()
        assert not participant_cursor.is_closed()
        await data.send(payload[::-1])
        assert (await message(participant_cursor, payload[::-1])).data == payload[::-1]
        await participant_cursor.close()
        await admitted_publication(core, publication)
        exchange = agent.data("foreign-exchange", core.TopicMode.ORDERED).publish()
        replies = agent.room(room).participant("publisher").data(
            "foreign-exchange", core.TopicMode.ORDERED,
        ).subscribe()

        async def send_until_reply():
            while True:
                await exchange.send(b"python-bytes-\0\xff")
                await asyncio.sleep(0.2)

        async with asyncio.TaskGroup() as group:
            sender = group.create_task(send_until_reply())
            reply = await message(replies, b"rust-ack:python-bytes-\0\xff", "publisher")
            assert isinstance(reply.sequence, int)
            sender.cancel()
        await replies.close()
        received_interest = agent.room(room).participant("publisher").video("production-camera").subscribe(
            core.VideoObservation(
                visible=True, height=96, min_height=0, min_fps=0, priority=1,
                playout_delay=core.PlayoutDelay.ADAPTIVE(),
            ),
        )
        received = received_interest.frames()
        frame = await received.next()
        assert isinstance(frame, native.VideoFrame), frame
        assert isinstance(frame.track, native.NativeReceiverInfo)
        assert frame.track.publication_id and frame.track.mid
        assert isinstance(frame.metadata, native.FrameMetadata)
        pixels = frame.frame
        assert isinstance(pixels, native.I420Frame)
        assert pixels.width == 160 and pixels.height == 96
        assert len(pixels.data) == 160 * 96 + 2 * 80 * 48
        assert isinstance(pixels.timestamp_us, int)
        retained = bytes(pixels.data)
        received.close()
        await received_interest.close()
        assert bytes(frame.frame.data) == retained, "owned pixels changed after reader and subscription close"
        interest = agent.room(room).participant("absent-publisher").audio("microphone").subscribe(
            core.AudioSubscriptionConfig(playout_delay=core.PlayoutDelay.ADAPTIVE()),
        )
        video_preferences = core.VideoObservation(
            visible=True, height=720, min_height=0, min_fps=0, priority=1,
            playout_delay=core.PlayoutDelay.ADAPTIVE(),
        )
        video_interest = agent.room(room).participant("absent-publisher").video("camera").subscribe(
            video_preferences,
        )
        assert not video_interest.is_closed(), "new foreign video interest is closed"
        video_preferences.visible = False
        await video_interest.configure(video_preferences)
        await video_interest.close()
        assert video_interest.is_closed(), "foreign video interest did not close"
        assert not interest.is_closed(), "video close ended the independent audio interest"
        audio_policy = core.AudioDemand(
            pinned=["absent-source"], selected=[],
            playout_delays=[], selector_delays=[],
        )
        all_audio = await agent.select_audio(audio_policy)
        audio_policy.pinned = []
        await all_audio.configure(audio_policy)
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
        await publication.close()
        assert publication.is_closed() and not camera.is_closed(), "publication close stopped producer"
        await camera.close()
        assert camera.is_closed(), "producer close did not retire input"
        await asyncio.wait_for(agent.close(), timeout=5)
        assert agent.state().status == core.AgentStatus.CLOSED
        assert agent.statistics() is None, "foreign native graph not retired"
        print("installed Python synchronous Agent/dynamic RTC/real SFU/live catalog/owned binary data/independent source/named media close PASS")
    finally:
        watch.close()
        watch_updates.close()
        await asyncio.wait_for(asyncio.gather(agent.close(), reader.close(), catalog_agent.close()), timeout=3)


async def admitted_publication(core, publication):
    updates = publication.updates()
    try:
        while True:
            state = await updates.next()
            assert state is not None and not state.closed, "publication closed before admission"
            assert state.admission.status != core.PublicationAdmissionStatus.REJECTED, state.admission
            if state.admission.status == core.PublicationAdmissionStatus.ADMITTED:
                return
    finally:
        updates.close()


async def publish_live(endpoint, token, room, package):
    core = importlib.import_module(f"{package}.pulsebeam_agent_core")
    native = importlib.import_module(f"{package}.pulsebeam_agent_native")
    agent = native.Agent(core.AgentConfig(
        endpoint=endpoint, token=token, log_level=core.LogLevel.WARN,
        retry=core.RetryPolicy(
            initial_delay_ms=100, maximum_delay_ms=1000, maximum_attempts=2,
        ),
    ))
    camera = await agent.create_video_input(native.VideoSourceConfig(
        encodings=[native.Encoding(
            rid=None, scale_down_by=1, max_bitrate_bps=None,
            max_framerate=None, temporal_layers=1,
        )], dependency_descriptor=True,
    ))
    publication = agent.video("camera").publish()
    await publication.set_source(camera)
    data = agent.data("frame-events", core.TopicMode.ORDERED).publish()
    agent.join(room)
    await admitted_publication(core, publication)

    async def frames():
        sequence = 0
        while True:
            await camera.submit(native.InputFrame.I420(frame=native.I420Frame(
                width=160, height=96, timestamp_us=await camera.capture_time_us(),
                data=bytes([180]) * (160 * 96) + bytes([90]) * (80 * 48)
                + bytes([180]) * (80 * 48), rotation_degrees=0,
            )))
            await data.send(bytes([0, 255, 128]) + sequence.to_bytes(8, "little"))
            sequence += 1
            # Live source cadence, not a retry of an accepted frame or payload.
            await asyncio.sleep(1 / 30)

    pump = asyncio.create_task(frames())
    stop = asyncio.create_task(asyncio.to_thread(sys.stdin.readline))
    try:
        done, _ = await asyncio.wait([pump, stop], return_when=asyncio.FIRST_COMPLETED)
        for task in done:
            task.result()
    finally:
        pump.cancel()
        await asyncio.gather(pump, return_exceptions=True)
        await publication.close()
        await camera.close()
        await agent.close()


def main():
    if len(sys.argv) == 7 and sys.argv[1] == "--witness":
        package = importlib.import_module("pulsebeam_agent")
        assert Path(package.__file__).is_relative_to("/opt/lib/python3.12/site-packages")
        asyncio.run(asyncio.wait_for(witness(sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5], sys.argv[6], package.__name__), timeout=25))
        return

    from installer import install
    from installer.destinations import SchemeDictionaryDestination
    from installer.sources import WheelFile

    wheels = list(Path(os.environ["PULSEBEAM_NATIVE_WHEEL"]).resolve(strict=True).glob("*.whl"))
    if len(wheels) != 1:
        raise RuntimeError(f"expected one native agent wheel, got {wheels}")
    with tempfile.TemporaryDirectory(prefix="native-foreign-") as temporary:
        site = Path(temporary) / "site"
        destination = SchemeDictionaryDestination(
            {key: str(site) for key in ("purelib", "platlib", "headers", "scripts", "data")},
            interpreter=sys.executable,
            script_kind="posix",
        )
        with WheelFile.open(wheels[0]) as wheel:
            install(wheel, destination, additional_metadata={"INSTALLER": b"pulsebeam-conformance"})
        baseline = os.environ.get("PULSEBEAM_BASELINE_ROOT")
        if baseline:
            shutil.copyfile(__file__, Path(temporary) / "witness.py")
            shutil.copyfile(os.environ["PULSEBEAM_BASELINE_CA_FILE"], Path(temporary) / "ca.pem")
            python_root = Path(sys.executable).resolve().parent.parent
            subprocess.run([
                "/usr/bin/bwrap", "--die-with-parent", "--ro-bind", baseline, "/", "--dev", "/dev", "--proc", "/proc",
                "--ro-bind", temporary, "/home", "--ro-bind", str(python_root), "/opt",
                "--ro-bind", str(site), "/opt/lib/python3.12/site-packages", "--chdir", "/home",
                "--clearenv", "--setenv", "PATH", "/usr/bin:/bin",
                "--setenv", "SSL_CERT_FILE", "/home/ca.pem", "--",
                "/opt/bin/python3.12", "/home/witness.py", "--witness", *sys.argv[1:],
            ], check=True, timeout=30)
        else:
            sys.path.insert(0, str(site))
            package = importlib.import_module("pulsebeam_agent")
            assert Path(package.__file__).is_relative_to(site)
            if len(sys.argv) == 5 and sys.argv[1] == "--publish":
                asyncio.run(asyncio.wait_for(publish_live(sys.argv[2], sys.argv[3], sys.argv[4], package.__name__), timeout=30))
            else:
                asyncio.run(asyncio.wait_for(witness(sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5], package.__name__), timeout=25))


if __name__ == "__main__":
    main()
