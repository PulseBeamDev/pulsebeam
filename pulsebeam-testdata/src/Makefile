INPUT_VIDEO = video.mov
INPUT_H264 = quarter_q_cbr.h264
OUT_DIR     = .
FPS         = 30
DURATION    = 360

# Bitrates in kbps (GStreamer x264enc uses kbit/sec)
BITRATE_F = 1250
BITRATE_H = 400
BITRATE_Q = 150

# Core structural settings shared across both VBR and CBR profiles
# key-int-max=2147483647 == one IDR at start, pure P-frames after
BASE_COMMON = tune=zerolatency \
              bframes=0 \
              b-adapt=false \
              key-int-max=2147483647 \
              speed-preset=ultrafast \
              byte-stream=true \
              aud=false \
              option-string="repeat-headers=1:open-gop=0"

# Specific rate-control modifiers
CBR_FLAGS   = pass=cbr nal-hrd=cbr
VBR_FLAGS   = # Intentionally blank. x264enc defaults to VBR/ABR when pass=cbr is omitted.

.PHONY: all clean prepare estimate

all: prepare \
     $(OUT_DIR)/full_f_vbr.h264 $(OUT_DIR)/full_f_cbr.h264 \
     $(OUT_DIR)/half_h_vbr.h264 $(OUT_DIR)/half_h_cbr.h264 \
     $(OUT_DIR)/quarter_q_vbr.h264 $(OUT_DIR)/quarter_q_cbr.h264

prepare:
	mkdir -p $(OUT_DIR)

# Trim to 45s — Handled via VBR for the mezzanine file to capture source complexity accurately
$(OUT_DIR)/trimmed.mp4: $(INPUT_VIDEO)
	gst-launch-1.0 -e \
		filesrc location=$< ! \
		decodebin ! \
		videorate ! \
		video/x-raw,framerate=$(FPS)/1 ! \
		x264enc $(BASE_COMMON) $(VBR_FLAGS) bitrate=4000 ! \
		video/x-h264,profile=baseline ! \
		mp4mux ! \
		filesink location=$@

# ==============================================================================
# HIGH LAYER (Full Resolution)
# ==============================================================================
$(OUT_DIR)/full_f_vbr.h264: $(OUT_DIR)/trimmed.mp4
	gst-launch-1.0 -e \
		filesrc location=$< ! \
		qtdemux ! h264parse ! avdec_h264 ! \
		videorate ! video/x-raw,framerate=$(FPS)/1 ! \
		x264enc $(BASE_COMMON) $(VBR_FLAGS) bitrate=$(BITRATE_F) ! \
		video/x-h264,profile=baseline,stream-format=byte-stream ! \
		filesink location=$@

$(OUT_DIR)/full_f_cbr.h264: $(OUT_DIR)/trimmed.mp4
	gst-launch-1.0 -e \
		filesrc location=$< ! \
		qtdemux ! h264parse ! avdec_h264 ! \
		videorate ! video/x-raw,framerate=$(FPS)/1 ! \
		x264enc $(BASE_COMMON) $(CBR_FLAGS) bitrate=$(BITRATE_F) vbv-buf-capacity=$(BITRATE_F) ! \
		video/x-h264,profile=baseline,stream-format=byte-stream ! \
		filesink location=$@

# ==============================================================================
# MID LAYER (360p)
# ==============================================================================
$(OUT_DIR)/half_h_vbr.h264: $(OUT_DIR)/trimmed.mp4
	gst-launch-1.0 -e \
		filesrc location=$< ! \
		qtdemux ! h264parse ! avdec_h264 ! \
		videorate ! video/x-raw,framerate=$(FPS)/1 ! \
		videoscale ! video/x-raw,height=360,pixel-aspect-ratio=1/1 ! \
		x264enc $(BASE_COMMON) $(VBR_FLAGS) bitrate=$(BITRATE_H) ! \
		video/x-h264,profile=baseline,stream-format=byte-stream ! \
		filesink location=$@

$(OUT_DIR)/half_h_cbr.h264: $(OUT_DIR)/trimmed.mp4
	gst-launch-1.0 -e \
		filesrc location=$< ! \
		qtdemux ! h264parse ! avdec_h264 ! \
		videorate ! video/x-raw,framerate=$(FPS)/1 ! \
		videoscale ! video/x-raw,height=360,pixel-aspect-ratio=1/1 ! \
		x264enc $(BASE_COMMON) $(CBR_FLAGS) bitrate=$(BITRATE_H) vbv-buf-capacity=$(BITRATE_H) ! \
		video/x-h264,profile=baseline,stream-format=byte-stream ! \
		filesink location=$@

# ==============================================================================
# LOW LAYER (180p)
# ==============================================================================
$(OUT_DIR)/quarter_q_vbr.h264: $(OUT_DIR)/trimmed.mp4
	gst-launch-1.0 -e \
		filesrc location=$< ! \
		qtdemux ! h264parse ! avdec_h264 ! \
		videorate ! video/x-raw,framerate=$(FPS)/1 ! \
		videoscale ! video/x-raw,height=180,pixel-aspect-ratio=1/1 ! \
		x264enc $(BASE_COMMON) $(VBR_FLAGS) bitrate=$(BITRATE_Q) ! \
		video/x-h264,profile=baseline,stream-format=byte-stream ! \
		filesink location=$@

$(OUT_DIR)/quarter_q_cbr.h264: $(OUT_DIR)/trimmed.mp4
	gst-launch-1.0 -e \
		filesrc location=$< ! \
		qtdemux ! h264parse ! avdec_h264 ! \
		videorate ! video/x-raw,framerate=$(FPS)/1 ! \
		videoscale ! video/x-raw,height=180,pixel-aspect-ratio=1/1 ! \
		x264enc $(BASE_COMMON) $(CBR_FLAGS) bitrate=$(BITRATE_Q) vbv-buf-capacity=$(BITRATE_Q) ! \
		video/x-h264,profile=baseline,stream-format=byte-stream ! \
		filesink location=$@

# ==============================================================================
# ANALYSIS & UTILITIES
# ==============================================================================
estimate:
	@if [ ! -f "$(INPUT_H264)" ]; then \
		echo "Error: Input file '$(INPUT_H264)' not found." >&2; \
		exit 1; \
	fi
	@echo "timestamp_seconds,frame_count,bitrate_kbps"
	@ffprobe -v error -framerate $(FPS) -i $(INPUT_H264) -show_entries packet=size -of compact=p=0:nk=1 | \
	awk \
		-v window=$(FPS) \
		-v fps=$(FPS) \
		'BEGIN { sum=0 } \
		{ \
			idx = NR % window; \
			sum -= history[idx]; \
			history[idx] = $$1; \
			sum += $$1; \
			if (NR >= window) { \
				timestamp = NR / fps; \
				printf "%.3f,%d,%.2f\n", timestamp, NR, (sum * 8) / 1000; \
			} \
		}'

clean:
	rm -f $(OUT_DIR)/trimmed.mp4 \
	      $(OUT_DIR)/*_vbr.h264 \
	      $(OUT_DIR)/*_cbr.h264
