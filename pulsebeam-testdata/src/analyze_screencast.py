import argparse
import statistics
import subprocess
from pathlib import Path


FPS = 30
WIDTH = 160
HEIGHT = 90
STATIC_PIXEL_DELTA = 0.5
STATIC_TARGET_FRACTION = 0.25


def command_output(command):
    return subprocess.check_output(command, stderr=subprocess.PIPE)


def source_motion(path):
    raw = command_output(
        [
            "ffmpeg",
            "-v",
            "error",
            "-i",
            path,
            "-an",
            "-vf",
            f"fps={FPS},scale={WIDTH}:{HEIGHT}:flags=area,format=gray",
            "-f",
            "rawvideo",
            "-",
        ]
    )
    frame_size = WIDTH * HEIGHT
    assert len(raw) % frame_size == 0
    frames = [raw[i : i + frame_size] for i in range(0, len(raw), frame_size)]
    assert frames

    deltas = [0.0]
    for previous, current in zip(frames, frames[1:]):
        difference = sum(abs(a - b) for a, b in zip(previous, current)) / frame_size
        deltas.append(difference)
    return deltas


def packet_sizes(path):
    output = command_output(
        [
            "ffprobe",
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "packet=size",
            "-of",
            "csv=p=0",
            path,
        ]
    ).decode()
    sizes = [int(line) for line in output.splitlines() if line.strip()]
    assert sizes
    return sizes


def scheduled_bitrate(path):
    sizes = packet_sizes(path)
    timestamps = [
        int(line)
        for line in Path(path).with_suffix(".timing").read_text().splitlines()
        if line.strip()
    ]
    assert len(sizes) == len(timestamps)
    bitrate = [0.0] * (timestamps[-1] // 1_000_000 + 1)
    for size, timestamp in zip(sizes, timestamps):
        bitrate[timestamp // 1_000_000] += size * 8 / 1000
    return sizes, bitrate, timestamps[-1] / 1_000_000


def per_second(values, reducer):
    return [reducer(values[i : i + FPS]) for i in range(0, len(values), FPS) if values[i : i + FPS]]


def parse_stream(value):
    try:
        name, target, path = value.split(":", 2)
        return name, int(target), path
    except ValueError as error:
        raise argparse.ArgumentTypeError("expected NAME:TARGET_KBPS:PATH") from error


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", required=True)
    parser.add_argument("--stream", action="append", type=parse_stream, required=True)
    args = parser.parse_args()

    motion = per_second(source_motion(args.source), statistics.mean)
    static_seconds = [index for index, delta in enumerate(motion) if delta <= STATIC_PIXEL_DELTA]
    assert static_seconds, "source contains no independently detected static seconds"

    print(
        f"source: {len(motion)}s, static: {len(static_seconds)}s "
        f"(mean pixel delta <= {STATIC_PIXEL_DELTA})"
    )
    print("layer target_kbps overall_kbps static_median static_p95 static_max status")

    failed = False
    for name, target_kbps, path in args.stream:
        sizes, bitrate, duration = scheduled_bitrate(path)
        aligned_static = [bitrate[index] for index in static_seconds if index < len(bitrate)]
        assert aligned_static, f"{name}: no static source seconds overlap encoded stream"
        ordered = sorted(aligned_static)
        p95 = ordered[min(len(ordered) - 1, int(len(ordered) * 0.95))]
        median = statistics.median(ordered)
        maximum = max(ordered)
        overall = sum(sizes) * 8 / 1000 / duration
        limit = target_kbps * STATIC_TARGET_FRACTION
        passed = median <= limit
        failed |= not passed
        print(
            f"{name:>5} {target_kbps:>11.0f} {overall:>12.1f} {median:>13.1f} "
            f"{p95:>10.1f} {maximum:>10.1f} {'PASS' if passed else 'FAIL'}"
        )

    if failed:
        raise SystemExit(
            f"static median must be <= {STATIC_TARGET_FRACTION:.0%} of each layer target"
        )


if __name__ == "__main__":
    main()
