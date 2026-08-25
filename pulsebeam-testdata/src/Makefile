# Source Configuration
SOURCE_URL   = https://download.blender.org/peach/bigbuckbunny_movies/big_buck_bunny_720p_h264.mov
# https://www.pexels.com/video/scenic-city-drive-at-sunset-from-car-dashboard-31901316/
INPUT_VIDEO  = driving.mp4
# INPUT_VIDEO = BigBuckBunny_720p.mov

INPUT_H264   = full_f_cbr.h264
SCREENCAST   = screencast.mp4
OUT_DIR      = .
FPS          = 30
# Chrome caps detail at 15fps but a real share peaks near 8: maintain-resolution sheds frames
# rather than pixels, so the ceiling is rarely reached.
SCREEN_FPS   = 8
SCREEN_IDLE_FPS = 0.5
# Production screen shares are 1080p (Chrome reports frameHeight=1080, frameWidth=1920).
SCREEN_HEIGHT = 1080
SCREEN_BITRATE = 2500

# Bitrates in kbps (GStreamer x264enc uses kbit/sec)
BITRATE_F = 1250
BITRATE_H = 400
BITRATE_Q = 150

# Core structural settings shared across CBR profile
BASE_COMMON = tune=zerolatency \
              bframes=0 \
              b-adapt=false \
              key-int-max=2147483647 \
              speed-preset=ultrafast \
              byte-stream=true \
              aud=false \
              rc-lookahead=0 \
              sliced-threads=false \
							threads=1 \
							# intra-refresh allows ammortizing complex scenes to a series of slightly larger P-frames instead of 
							# generating a big IDR frame. This means much more stable bitrate and lower latency.
							#
							# https://news.ycombinator.com/item?id=25970843
							# http://www.chaneru.com/Roku/HLS/X264_Settings.htm#intra-refresh
							intra-refresh=true \
              option-string="repeat-headers=1:open-gop=0:scenecut=0"

# Specific rate-control modifiers
CBR_FLAGS = pass=cbr vbv-buf-capacity=200

.PHONY: all clean prepare estimate download distclean screenshare screenshare-analyze

all: prepare $(INPUT_VIDEO) \
     $(OUT_DIR)/full_f_cbr.h264 \
     $(OUT_DIR)/half_h_cbr.h264 \
     $(OUT_DIR)/quarter_q_cbr.h264

prepare:
	mkdir -p $(OUT_DIR)

# Target to download the source video if it doesn't exist
download: $(INPUT_VIDEO)

$(INPUT_VIDEO):
	@echo "Downloading source video from Blender network..."
	curl -L -o $(INPUT_VIDEO) $(SOURCE_URL)

# ==============================================================================
# HIGH LAYER (Full Resolution)
# ==============================================================================
$(OUT_DIR)/full_f_cbr.h264: $(INPUT_VIDEO)
	gst-launch-1.0 -e \
		filesrc location=$< ! \
		qtdemux ! h264parse ! avdec_h264 ! \
		videorate ! video/x-raw,framerate=$(FPS)/1 ! \
		videoscale ! video/x-raw,height=720,pixel-aspect-ratio=1/1 ! \
		x264enc $(BASE_COMMON) $(CBR_FLAGS) bitrate=$(BITRATE_F) ! \
		video/x-h264,profile=baseline,stream-format=byte-stream ! \
		filesink location=$@

# ==============================================================================
# MID LAYER (360p)
# ==============================================================================
$(OUT_DIR)/half_h_cbr.h264: $(INPUT_VIDEO)
	gst-launch-1.0 -e \
		filesrc location=$< ! \
		qtdemux ! h264parse ! avdec_h264 ! \
		videorate ! video/x-raw,framerate=$(FPS)/1 ! \
		videoscale ! video/x-raw,height=360,pixel-aspect-ratio=1/1 ! \
		x264enc $(BASE_COMMON) $(CBR_FLAGS) bitrate=$(BITRATE_H) ! \
		video/x-h264,profile=baseline,stream-format=byte-stream ! \
		filesink location=$@

# ==============================================================================
# LOW LAYER (180p)
# ==============================================================================
$(OUT_DIR)/quarter_q_cbr.h264: $(INPUT_VIDEO)
	gst-launch-1.0 -e \
		filesrc location=$< ! \
		qtdemux ! h264parse ! avdec_h264 ! \
		videorate ! video/x-raw,framerate=$(FPS)/1 ! \
		videoscale ! video/x-raw,height=180,pixel-aspect-ratio=1/1 ! \
		x264enc $(BASE_COMMON) $(CBR_FLAGS) bitrate=$(BITRATE_Q) ! \
		video/x-h264,profile=baseline,stream-format=byte-stream ! \
		filesink location=$@

screenshare: $(OUT_DIR)/screen_f_vbr.h264

$(OUT_DIR)/screen_f_vbr.h264: $(SCREENCAST)
	python3 encode_screencast.py --input $< --output $@ --height $(SCREEN_HEIGHT) --bitrate $(SCREEN_BITRATE) --fps $(SCREEN_FPS) --idle-fps $(SCREEN_IDLE_FPS)

screenshare-analyze: screenshare
	python3 analyze_screencast.py \
		--source $(SCREENCAST) \
		--stream f:$(SCREEN_BITRATE):$(OUT_DIR)/screen_f_vbr.h264

# ==============================================================================
# ANALYSIS & UTILITIES
# ==============================================================================

# Prints one line per access unit (frame) with its size in bytes.
# NOTE: fakesink's per-buffer "last-message" is only printed via gst-launch's
# deep-notify hook, which requires -v (verbose). -q suppresses it entirely,
# so grep matches nothing and `make` reports Error 1 with no visible cause.
estimate:
	@if [ ! -f "$(INPUT_H264)" ]; then \
		echo "Error: Input file '$(INPUT_H264)' not found." >&2; \
		exit 1; \
	fi
	@gst-launch-1.0 -v \
		filesrc location=$(INPUT_H264) ! \
		h264parse ! \
		fakesink silent=false 2>&1 | \
		grep -oP '\(\K[0-9]+(?=\s*bytes)' || \
		{ echo "Error: no frame sizes captured — check gst-launch output format." >&2; exit 1; }

clean:
	rm -f $(OUT_DIR)/trimmed.mp4 \
	      $(OUT_DIR)/*_cbr.h264 \
	      $(OUT_DIR)/screen_*_vbr.h264 \
	      $(OUT_DIR)/screen_*_vbr.timing

# Deep clean to drop the large source video file
distclean: clean
	rm -f $(INPUT_VIDEO)
