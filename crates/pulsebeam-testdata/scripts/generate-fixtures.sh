#!/usr/bin/env bash
set -euo pipefail

source_video="${1:?source video path is required}"
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

test -f "$source_video" || { echo "source video not found: $source_video" >&2; exit 1; }

generate_layer() {
    local height="$1"
    local bitrate="$2"
    local output="$3"
    gst-launch-1.0 -e \
        filesrc location="$source_video" ! \
        qtdemux ! h264parse ! avdec_h264 ! \
        videorate ! video/x-raw,framerate=30/1 ! \
        videoscale ! video/x-raw,height="$height",pixel-aspect-ratio=1/1 ! \
        x264enc tune=zerolatency bframes=0 b-adapt=false key-int-max=2147483647 \
          speed-preset=ultrafast byte-stream=true aud=false rc-lookahead=0 \
          sliced-threads=false threads=1 intra-refresh=true pass=cbr \
          vbv-buf-capacity=200 bitrate="$bitrate" \
          option-string="repeat-headers=1:open-gop=0:scenecut=0" ! \
        video/x-h264,profile=baseline,stream-format=byte-stream ! \
        filesink location="$output"
}

generate_layer 720 1250 src/full_f_cbr.h264
generate_layer 360 400 src/half_h_cbr.h264
generate_layer 180 150 src/quarter_q_cbr.h264
