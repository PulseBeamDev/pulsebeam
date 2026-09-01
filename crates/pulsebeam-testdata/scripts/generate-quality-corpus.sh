#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root/src"

fps=30
seconds=6
frames=180
epoch_frames=90
font="${CORPUS_FONT:-/usr/share/fonts/google-noto-vf/NotoSans[wght].ttf}"

generate_video() {
    local source_id="$1" source_filter="$2" height="$3" width="$4"
    local x_speed="$5" box_width="$6" y_base="$7" y_speed="$8"
    local y_span="$9" box_height="${10}" color="${11}" label_width="${12}"
    local label_height="${13}" font_size="${14}" bitrate="${15}" buffer="${16}"
    local stem="quality_s${source_id}_${height}p"

    ffmpeg -hide_banner -loglevel error -y \
      -f lavfi -i "${source_filter}=size=1280x720:rate=${fps}:duration=${seconds}" \
      -vf "scale=${width}:${height}:flags=lanczos,drawbox=x='mod(t*${x_speed},iw-${box_width})':y='${y_base}+mod(t*${y_speed},ih-${y_span})':w=${box_width}:h=${box_height}:color=${color}@1:t=fill,drawbox=x=8:y=8:w=${label_width}:h=${label_height}:color=black@1:t=fill,drawtext=fontfile='${font}':text='S${source_id} L${height} E%{eif\\:floor(n/${epoch_frames})\\:d} F%{eif\\:mod(n\\,${epoch_frames})\\:d}':x=14:y=14:fontsize=${font_size}:fontcolor=white" \
      -frames:v "$frames" -pix_fmt yuv420p -c:v libx264 -profile:v baseline -level:v 3.1 \
      -preset ultrafast -tune zerolatency -b:v "${bitrate}k" -maxrate "${bitrate}k" -bufsize "${buffer}k" \
      -g "$epoch_frames" -keyint_min "$epoch_frames" -sc_threshold 0 \
      -x264-params 'repeat-headers=1:aud=1:bframes=0:open-gop=0:threads=1' -f h264 "${stem}.h264"

    ffmpeg -hide_banner -loglevel error -i "${stem}.h264" -pix_fmt yuv420p -f rawvideo - \
      | zstd -19 -T1 -f -o "${stem}.yuv420p.zst"
}

generate_video 0 testsrc2 180 320 390 48 20 210 60 32 white 225 42 18 180 360
generate_video 0 testsrc2 360 640 780 96 40 420 120 64 white 450 84 36 600 1200
generate_video 0 testsrc2 720 1280 1560 192 80 840 240 128 white 900 168 72 1800 3600
generate_video 1 smptebars 180 320 330 48 28 180 64 32 yellow 225 42 18 180 360
generate_video 1 smptebars 360 640 660 96 56 360 128 64 yellow 450 84 36 600 1200
generate_video 1 smptebars 720 1280 1320 192 112 720 256 128 yellow 900 168 72 1800 3600

ffmpeg -hide_banner -loglevel error -y -f lavfi \
  -i 'aevalsrc=if(between(t\,2\,3)+between(t\,5\,6)\,0\,(0.16+0.02*mod(floor(t/0.02)\,4))*sin(2*PI*(697+73*mod(floor(t/0.02)\,2))*t)):s=48000:d=6' \
  -ac 1 -ar 48000 -f s16le - \
  | gst-launch-1.0 -q fdsrc ! rawaudioparse format=pcm pcm-format=s16le sample-rate=48000 num-channels=1 ! \
      opusenc audio-type=voice bitrate=48000 bitrate-type=vbr frame-size=20 dtx=true ! oggmux ! \
      filesink location=quality_a0_48k_mono.opus

ffmpeg -hide_banner -loglevel error -y -f lavfi \
  -i 'aevalsrc=if(between(t\,2\,3)+between(t\,5\,6)\,0\,(0.14+0.025*mod(floor(t/0.02)\,4))*sin(2*PI*(941+268*mod(floor(t/0.02)\,2))*t)):s=48000:d=6' \
  -ac 1 -ar 48000 -f s16le - \
  | gst-launch-1.0 -q fdsrc ! rawaudioparse format=pcm pcm-format=s16le sample-rate=48000 num-channels=1 ! \
      opusenc audio-type=voice bitrate=48000 bitrate-type=vbr frame-size=20 dtx=true ! oggmux ! \
      filesink location=quality_a1_48k_mono.opus

for source_id in 0 1; do
    ffmpeg -hide_banner -loglevel error -i "quality_a${source_id}_48k_mono.opus" \
      -ac 1 -ar 48000 -f s16le - \
      | zstd -19 -T1 -f -o "quality_a${source_id}_48k_mono.s16le.zst"
done
