# Source Configuration
SOURCE_URL   = https://download.blender.org/peach/bigbuckbunny_movies/big_buck_bunny_720p_h264.mov
INPUT_VIDEO  = BigBuckBunny_720p.mov

INPUT_H264   = full_f_cbr.h264
OUT_DIR      = .
FPS          = 30
DURATION     = 360

# Bitrates in kbps (GStreamer x264enc uses kbit/sec)
BITRATE_F = 1250
BITRATE_H = 400
BITRATE_Q = 150

# Core structural settings shared across both VBR and CBR profiles
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
              option-string="repeat-headers=1:open-gop=0:scenecut=0:intra-refresh=0"

# Specific rate-control modifiers
CBR_FLAGS = pass=cbr nal-hrd=cbr vbv-buf-capacity=1000
VBR_FLAGS   = # Intentionally blank. x264enc defaults to VBR/ABR when pass=cbr is omitted.

.PHONY: all clean prepare estimate download distclean

all: prepare $(INPUT_VIDEO) \
     $(OUT_DIR)/full_f_vbr.h264 $(OUT_DIR)/full_f_cbr.h264 \
     $(OUT_DIR)/half_h_vbr.h264 $(OUT_DIR)/half_h_cbr.h264 \
     $(OUT_DIR)/quarter_q_vbr.h264 $(OUT_DIR)/quarter_q_cbr.h264

prepare:
	mkdir -p $(OUT_DIR)

# Target to download the source video if it doesn't exist
download: $(INPUT_VIDEO)

$(INPUT_VIDEO):
	@echo "Downloading source video from Blender network..."
	curl -L -o $(INPUT_VIDEO) $(SOURCE_URL)

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
		x264enc $(BASE_COMMON) $(CBR_FLAGS) bitrate=$(BITRATE_F) ! \
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
		x264enc $(BASE_COMMON) $(CBR_FLAGS) bitrate=$(BITRATE_H) ! \
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
		x264enc $(BASE_COMMON) $(CBR_FLAGS) bitrate=$(BITRATE_Q) ! \
		video/x-h264,profile=baseline,stream-format=byte-stream ! \
		filesink location=$@

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

# Same as estimate, but also prints stdev/CV, and separates the largest frame
# (likely a keyframe) from the steady-state P-frames for a cleaner
# apples-to-apples "tightness" comparison against Chrome's trace.
estimate-summary:
	@if [ ! -f "$(INPUT_H264)" ]; then \
		echo "Error: Input file '$(INPUT_H264)' not found." >&2; \
		exit 1; \
	fi
	@gst-launch-1.0 -v \
		filesrc location=$(INPUT_H264) ! \
		h264parse ! \
		fakesink silent=false 2>&1 | \
		grep -oP '\(\K[0-9]+(?=\s*bytes)' | \
		awk '{ \
			print; \
			sizes[n]=$$1; sum+=$$1; n++; \
			if ($$1 > max) { max=$$1; maxidx=n-1 } \
		} \
		END { \
			if (!n) { print "no frames captured" > "/dev/stderr"; exit 1 } \
			mean = sum/n; \
			for (i=0;i<n;i++) { d=sizes[i]-mean; ss+=d*d } \
			stdev = sqrt(ss/n); \
			sum2=0; n2=0; \
			for (i=0;i<n;i++) { if (i!=maxidx) { sum2+=sizes[i]; n2++ } } \
			mean2 = n2 ? sum2/n2 : 0; \
			ss2=0; \
			for (i=0;i<n;i++) { if (i!=maxidx) { d=sizes[i]-mean2; ss2+=d*d } } \
			stdev2 = n2 ? sqrt(ss2/n2) : 0; \
			printf "---\n"; \
			printf "frames=%d  mean=%.1f  stdev=%.1f  cv=%.1f%%\n", n, mean, stdev, (stdev/mean)*100; \
			printf "largest frame: idx=%d size=%d (likely keyframe)\n", maxidx, max; \
			printf "excl. largest: frames=%d  mean=%.1f  stdev=%.1f  cv=%.1f%%\n", n2, mean2, stdev2, n2?(stdev2/mean2)*100:0; \
		}'

clean:
	rm -f $(OUT_DIR)/trimmed.mp4 \
	      $(OUT_DIR)/*_vbr.h264 \
	      $(OUT_DIR)/*_cbr.h264

# Deep clean to drop the large source video file
distclean: clean
	rm -f $(INPUT_VIDEO)
