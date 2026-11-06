"""Generated Python/dynamic RTC boundary, using the Rust witness's real SFU."""

import asyncio
import importlib
import os
from pathlib import Path
import shutil
import subprocess
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
        events = agent.events()
        topic_publisher = core.TopicPublisher(name="foreign-exchange", mode=core.TopicMode.ORDERED)
        topic_subscriber = core.TopicSubscriber(name="foreign-exchange", mode=core.TopicMode.ORDERED)
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
                topic_publishers=[topic_publisher],
                topic_subscribers=[topic_subscriber],
            )
        )
        while True:
            update = await updates.next()
            assert update.is_SNAPSHOT(), f"lost foreign snapshot observation: {update}"
            snapshot = update.snapshot
            assert not snapshot.connection.is_TERMINAL_FAILURE(), snapshot.failure
            if (snapshot.connection.is_CONNECTED() and snapshot.participant_id
                    and any(item.connected and item.data_track_id for item in snapshot.topics.publishers)
                    and any(item.connected and item.publishers for item in snapshot.topics.subscribers)):
                break
        assert snapshot.generation is not None
        assert isinstance(snapshot.generation, int)
        await camera.ready()
        async def send_until_reply():
            while True:
                await agent.send_topic(topic_publisher, b"python-bytes-\0\xff")
                await asyncio.sleep(0.2)

        async with asyncio.TaskGroup() as group:
            sender = group.create_task(send_until_reply())
            while True:
                update = await events.next()
                assert not update.is_LAGGED() and not update.is_CLOSED(), update
                if not update.is_EVENT() or not update.event.is_CORE():
                    continue
                notification = update.event.notification
                if not notification.is_TOPIC() or not notification.notification.is_MESSAGE():
                    continue
                message = notification.notification.message
                if message.name == "foreign-exchange" and message.payload == b"rust-ack:python-bytes-\0\xff":
                    assert isinstance(message.sequence, int)
                    break
            sender.cancel()
        received_interest = await agent.video_subscription(
            core.TrackSelector(participant_id="publisher", label="production-camera"),
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
    if len(sys.argv) == 4 and sys.argv[1] == "--witness":
        package = importlib.import_module("pulsebeam_agent")
        assert Path(package.__file__).is_relative_to("/opt/lib/python3.12/site-packages")
        asyncio.run(asyncio.wait_for(witness(sys.argv[2], sys.argv[3], package.__name__), timeout=25))
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
            asyncio.run(asyncio.wait_for(witness(sys.argv[1], sys.argv[2], package.__name__), timeout=25))


if __name__ == "__main__":
    main()
