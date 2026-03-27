INPUT_VIDEO = video.mp4
OUT_DIR     = .
FPS         = 30
DURATION    = 45

# Bitrates in kbps (GStreamer x264enc uses kbit/sec)
BITRATE_F = 1250
BITRATE_H = 400
BITRATE_Q = 150

# Shared x264enc properties
# pass=cbr + nal-hrd=cbr is all that's needed for strict CBR
# key-int-max=2147483647 == one IDR at start, pure P-frames after
COMMON = pass=cbr \
         nal-hrd=cbr \
         tune=zerolatency \
         bframes=0 \
         b-adapt=false \
         key-int-max=2147483647 \
         speed-preset=ultrafast \
         byte-stream=true \
         aud=false \
         option-string="repeat-headers=1:open-gop=0"

.PHONY: all clean prepare

all: prepare $(OUT_DIR)/full_f.h264 $(OUT_DIR)/half_h.h264 $(OUT_DIR)/quarter_q.h264

prepare:
	mkdir -p $(OUT_DIR)

# Trim to 45s — decode only, no quality loss from re-encode
$(OUT_DIR)/trimmed.mp4: $(INPUT_VIDEO)
	gst-launch-1.0 -e \
		filesrc location=$< ! \
		decodebin ! \
		videorate ! \
		video/x-raw,framerate=$(FPS)/1 ! \
		x264enc pass=cbr nal-hrd=cbr tune=zerolatency \
			bframes=0 b-adapt=false \
			key-int-max=2147483647 \
			speed-preset=ultrafast \
			byte-stream=true aud=false \
			bitrate=4000 \
			option-string="repeat-headers=1:open-gop=0" ! \
		video/x-h264,profile=baseline,stream-format=byte-stream ! \
		mp4mux ! \
		filesink location=$@

# High layer — 1250kbps, native resolution
$(OUT_DIR)/full_f.h264: $(OUT_DIR)/trimmed.mp4
	gst-launch-1.0 -e \
		filesrc location=$< ! \
		qtdemux ! h264parse ! avdec_h264 ! \
		videorate ! video/x-raw,framerate=$(FPS)/1 ! \
		x264enc $(COMMON) bitrate=$(BITRATE_F) ! \
		video/x-h264,profile=baseline,stream-format=byte-stream ! \
		filesink location=$@

# Mid layer — 400kbps, scale to 360p
$(OUT_DIR)/half_h.h264: $(OUT_DIR)/trimmed.mp4
	gst-launch-1.0 -e \
		filesrc location=$< ! \
		qtdemux ! h264parse ! avdec_h264 ! \
		videorate ! video/x-raw,framerate=$(FPS)/1 ! \
		videoscale ! video/x-raw,height=360,pixel-aspect-ratio=1/1 ! \
		x264enc $(COMMON) bitrate=$(BITRATE_H) ! \
		video/x-h264,profile=baseline,stream-format=byte-stream ! \
		filesink location=$@

# Low layer — 150kbps, scale to 180p
$(OUT_DIR)/quarter_q.h264: $(OUT_DIR)/trimmed.mp4
	gst-launch-1.0 -e \
		filesrc location=$< ! \
		qtdemux ! h264parse ! avdec_h264 ! \
		videorate ! video/x-raw,framerate=$(FPS)/1 ! \
		videoscale ! video/x-raw,height=180,pixel-aspect-ratio=1/1 ! \
		x264enc $(COMMON) bitrate=$(BITRATE_Q) ! \
		video/x-h264,profile=baseline,stream-format=byte-stream ! \
		filesink location=$@

clean:
	rm -f $(OUT_DIR)/trimmed.mp4 \
	      $(OUT_DIR)/full_f.h264 \
	      $(OUT_DIR)/half_h.h264 \
	      $(OUT_DIR)/quarter_q.h264
