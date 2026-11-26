"""Exact camera, segmented collection recording, and mixed DATA consumers."""

import asyncio

from pulsebeam_agent import pulsebeam_agent_core as core
from pulsebeam_agent import pulsebeam_agent_native as native


async def exact_camera(room: native.RoomRef, process):
    camera = room.participant("alice").video("camera").subscribe()
    try:
        frames = camera.frames()
        try:
            while (frame := await frames.next()) is not None:
                process(frame)
        finally:
            frames.close()
    finally:
        await camera.close()


async def close_while_waiting(room: native.RoomRef):
    camera = room.participant("absent-publisher").video("camera").subscribe()
    frames = camera.frames()
    first = asyncio.create_task(frames.next())
    try:
        await asyncio.sleep(0)
        assert not first.done(), "no-source read did not wait"
        await camera.close()
        assert await first is None
    finally:
        frames.close()
        await camera.close()
        first.cancel()
        await asyncio.gather(first, return_exceptions=True)


async def record_segment(frames, publication_id, open_segment, report_failure):
    try:
        first = await frames.next()
        if first is None:
            return
        with open_segment(publication_id) as segment:
            segment.write(first)
            while (frame := await frames.next()) is not None:
                segment.write(frame)
    except native.NativeError.Operation as failure:
        if failure.info.code != core.ErrorCode.CONTINUITY_LOST:
            raise
        report_failure(publication_id, failure.info)


async def record_cameras(room: native.RoomRef, open_segment, report_failure):
    cameras = room.video(core.TrackFilter(track_id="camera")).subscribe()
    updates = None
    changed = None
    readers = {}
    try:
        updates = cameras.updates()
        changed = asyncio.create_task(updates.next())
        while True:
            snapshot = cameras.state()
            if snapshot.closed:
                break
            current = {entry.publication_id: entry for entry in snapshot.tracks}
            for publication_id, (frames, segment) in list(readers.items()):
                if publication_id not in current or segment.done():
                    del readers[publication_id]
                    frames.close()
                    await segment
            for publication_id, entry in current.items():
                if publication_id in readers:
                    continue
                try:
                    frames = entry.track.frames()
                except native.NativeError.Operation as failure:
                    # A terminal notification can arrive before the owner's next snapshot.
                    if failure.info.code in (core.ErrorCode.REPLACED, core.ErrorCode.CLOSED):
                        continue
                    raise
                readers[publication_id] = (
                    frames,
                    asyncio.create_task(
                        record_segment(frames, publication_id, open_segment, report_failure)
                    ),
                )
            await asyncio.wait(
                [changed, *(segment for _, segment in readers.values())],
                return_when=asyncio.FIRST_COMPLETED,
            )
            if changed.done():
                if changed.result() is None:
                    break
                changed = asyncio.create_task(updates.next())
    finally:
        if updates is not None:
            updates.close()
        pending = []
        if changed is not None:
            changed.cancel()
            pending.append(changed)
        for frames, segment in readers.values():
            frames.close()
            segment.cancel()
            pending.append(segment)
        await asyncio.gather(*pending, return_exceptions=True)
        await cameras.close()


async def replace_metrics(agent: native.Agent, use_publication):
    track = agent.data("metrics")
    for delivery in (core.DataDelivery.RELIABLE, core.DataDelivery.LATEST):
        publication = track.publish(core.DataPublishOptions(delivery=delivery))
        try:
            await use_publication(publication)
        finally:
            await publication.close()


async def receive_mixed_data(room: native.RoomRef, deliver, source_failed):
    messages = room.data(core.TrackFilter(track_id="metrics")).subscribe()
    try:
        while (event := await messages.next()) is not None:
            if event.is_MESSAGE():
                deliver(event.participant_id, event.track_id, event.publication_id, event.data)
            elif event.is_SOURCE_FAILED():
                source_failed(
                    event.participant_id, event.track_id, event.publication_id, event.error
                )
    finally:
        await messages.close()


async def receive_exact_metrics(room: native.RoomRef, deliver, continuity_lost):
    messages = room.participant("alice").data("metrics").subscribe()
    try:
        while (event := await messages.next()) is not None:
            if event.is_MESSAGE():
                deliver(event.participant_id, event.track_id, event.publication_id, event.data)
    except native.NativeError.Operation as failure:
        if failure.info.code != core.ErrorCode.CONTINUITY_LOST:
            raise
        continuity_lost(failure.info)
    finally:
        await messages.close()
