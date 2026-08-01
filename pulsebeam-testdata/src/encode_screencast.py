import argparse
import pathlib
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--input", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--height", required=True, type=int)
    parser.add_argument("--bitrate", required=True, type=int)
    parser.add_argument("--fps", default=30, type=int)
    args = parser.parse_args()
    output = pathlib.Path(args.output)
    heartbeat = max(1, args.fps // 2)
    selection = f"select='gt(scene,0.001)+not(mod(n,{heartbeat}))',scale=-2:{args.height}:flags=lanczos"

    with tempfile.TemporaryDirectory() as directory:
        container = pathlib.Path(directory) / "screen.mkv"
        subprocess.run(
            [
                "ffmpeg", "-v", "error", "-y", "-i", args.input, "-an", "-vf", selection,
                "-fps_mode", "vfr", "-c:v", "libx264", "-preset", "veryfast", "-tune",
                "zerolatency", "-profile:v", "baseline", "-bf", "0", "-g", "3000",
                "-sc_threshold", "0", "-crf", "30", "-maxrate", f"{args.bitrate}k",
                "-bufsize", f"{args.bitrate * 3 // 5}k", "-x264-params",
                "nal-hrd=none:force-cfr=0:repeat-headers=1", str(container),
            ],
            check=True,
        )
        subprocess.run(
            [
                "ffmpeg", "-v", "error", "-y", "-i", str(container), "-an", "-c:v", "copy",
                "-bsf:v", "h264_mp4toannexb", "-f", "h264", str(output),
            ],
            check=True,
        )
        probe = subprocess.check_output(
            [
                "ffprobe", "-v", "error", "-select_streams", "v:0", "-show_entries",
                "packet=pts_time", "-of", "csv=p=0", str(container),
            ],
            text=True,
        )
    timestamps = [round(float(line) * 1_000_000) for line in probe.splitlines() if line.strip()]
    assert timestamps
    assert all(a < b for a, b in zip(timestamps, timestamps[1:]))
    output.with_suffix(".timing").write_text("\n".join(map(str, timestamps)) + "\n")


if __name__ == "__main__":
    main()
