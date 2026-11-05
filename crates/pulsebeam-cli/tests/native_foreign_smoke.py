"""Generated Python/dynamic RTC boundary, using the Rust witness's real SFU."""

import asyncio
import importlib
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


async def witness(endpoint, token, reader_token, package):
    core = importlib.import_module(f"{package}.pulsebeam_agent_core")
    native = importlib.import_module(f"{package}.pulsebeam_agent_native")
    agent = await native.Agent.create(
        core.AgentConfig(
            endpoint=endpoint,
            token=token,
            retry=core.RetryPolicy(
                initial_delay_ms=100, maximum_delay_ms=1000, maximum_attempts=2
            ),
            log_level=core.LogLevel.WARN,
        )
    )
    reader = await native.Agent.create(core.AgentConfig(
        endpoint=endpoint, token=reader_token,
        retry=core.RetryPolicy(
            initial_delay_ms=100, maximum_delay_ms=1000, maximum_attempts=2,
        ),
        log_level=core.LogLevel.WARN,
    ))
    try:
        updates = agent.snapshots()
        reader_updates = reader.snapshots()
        data = agent.data("foreign-bytes", core.TopicMode.ORDERED)
        room_cursor = await reader.data("foreign-bytes", core.TopicMode.ORDERED).receive()
        participant_cursor = await reader.participant("foreign").data(
            "foreign-bytes", core.TopicMode.ORDERED,
        ).receive()
        await reader.connect()
        camera = await agent.create_video_input(native.VideoSourceConfig(
            encodings=[native.Encoding(
                rid=None, scale_down_by=1, max_bitrate_bps=None,
                max_framerate=None, temporal_layers=1,
            )], dependency_descriptor=True,
        ))
        publication = await agent.video("foreign-camera").publish(
            camera, core.PublicationConfig(enabled=True),
        )
        await agent.connect()
        while True:
            update = await updates.next()
            assert update.is_SNAPSHOT(), f"lost foreign snapshot observation: {update}"
            snapshot = update.snapshot
            assert not snapshot.connection.is_TERMINAL_FAILURE(), snapshot.failure
            if snapshot.connection.is_CONNECTED() and snapshot.participant_id:
                break
        assert snapshot.generation is not None
        while True:
            update = await reader_updates.next()
            assert update.is_SNAPSHOT(), f"lost reader observation: {update}"
            assert not update.snapshot.connection.is_TERMINAL_FAILURE()
            if update.snapshot.connection.is_CONNECTED():
                break
        setup = bytes([0, 255, 128, 0])

        async def message(cursor, expected, participant_id="foreign"):
            while True:
                update = await cursor.next()
                assert update.is_EVENT() and update.event.is_MESSAGE(), update
                event = update.event
                assert event.participant_id == participant_id and event.track_id
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
        await publication.ready()
        exchange = agent.data("foreign-exchange", core.TopicMode.ORDERED)
        replies = await agent.participant("publisher").data(
            "foreign-exchange", core.TopicMode.ORDERED,
        ).receive()

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
        received_interest = await agent.participant("publisher").video("production-camera").subscribe(
            core.VideoObservation(
                visible=True, height=96, min_height=0, min_fps=0, priority=1,
                playout_delay=core.PlayoutDelay.ADAPTIVE(),
            ),
        )
        received = await received_interest.observe(native.ReceiveMode.DECODED)
        while True:
            update = await received.next()
            assert not update.is_CLOSED() and not update.is_LAGGED(), update
            if update.is_FRAME() and update.frame.frame.is_I420():
                pixels = update.frame.frame.frame
                assert pixels.width == 160 and pixels.height == 96
                assert len(pixels.data) == 160 * 96 + 2 * 80 * 48
                assert isinstance(pixels.timestamp_us, int)
                retained = bytes(pixels.data)
                break
        await received.close()
        await received_interest.close()
        assert bytes(pixels.data) == retained, "owned pixels changed after observation close"
        interest = await agent.participant("absent-publisher").audio("microphone").subscribe(
            core.AudioSubscriptionConfig(playout_delay=core.PlayoutDelay.ADAPTIVE()),
        )
        video_preferences = core.VideoObservation(
            visible=True, height=720, min_height=0, min_fps=0, priority=1,
            playout_delay=core.PlayoutDelay.ADAPTIVE(),
        )
        video_interest = await agent.participant("absent-publisher").video("camera").subscribe(
            video_preferences,
        )
        assert not video_interest.is_closed(), "new foreign video interest is closed"
        video_preferences.visible = False
        await video_interest.configure(video_preferences)
        await video_interest.close()
        assert video_interest.is_closed(), "foreign video interest did not close"
        assert not interest.is_closed(), "video close ended the independent audio interest"
        audio_policy = core.AudioDemand(
            pinned=[], selected=[], automatic=True,
            playout_delays=[], selector_delays=[],
        )
        all_audio = await agent.select_audio(audio_policy)
        audio_policy.automatic = False
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
        await asyncio.wait_for(agent.disconnect(), timeout=5)
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
        assert publication.is_closed() and not camera.is_closed(), "publication close stopped producer"
        await camera.close()
        assert camera.is_closed(), "producer close did not retire input"
        print("installed Python Agent.create/dynamic RTC/real SFU/owned binary data/independent source/named media close PASS")
    finally:
        await asyncio.wait_for(asyncio.gather(agent.close(), reader.close()), timeout=3)


def main():
    if len(sys.argv) == 5 and sys.argv[1] == "--witness":
        package = importlib.import_module("pulsebeam_agent")
        assert Path(package.__file__).is_relative_to("/opt/lib/python3.12/site-packages")
        asyncio.run(asyncio.wait_for(witness(sys.argv[2], sys.argv[3], sys.argv[4], package.__name__), timeout=25))
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
            asyncio.run(asyncio.wait_for(witness(sys.argv[1], sys.argv[2], sys.argv[3], package.__name__), timeout=25))


if __name__ == "__main__":
    main()
