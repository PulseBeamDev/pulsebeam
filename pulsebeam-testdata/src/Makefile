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
BITRATE_F = 900
BITRATE_H = 280
BITRATE_Q = 100

QUALITY_WIDTH = 320
QUALITY_HEIGHT = 180
QUALITY_FPS = 30
QUALITY_SECONDS = 3
QUALITY_VIDEO = quality_320x180_30.h264
QUALITY_VIDEO_REFERENCE = quality_320x180_30.yuv
QUALITY_AUDIO = quality_48k_mono.opus
QUALITY_AUDIO_REFERENCE = quality_48k_mono.s16le

CORPUS_FPS = 30
CORPUS_SECONDS = 6
CORPUS_FRAMES = 180
CORPUS_EPOCH_FRAMES = 90
CORPUS_FONT ?= /usr/share/fonts/google-noto-vf/NotoSans[wght].ttf
CORPUS_VIDEO = \
	quality_s0_180p.h264 quality_s0_360p.h264 quality_s0_720p.h264 \
	quality_s1_180p.h264 quality_s1_360p.h264 quality_s1_720p.h264
CORPUS_VIDEO_REFERENCE = $(CORPUS_VIDEO:.h264=.yuv420p.zst)
CORPUS_AUDIO = quality_a0_48k_mono.opus quality_a1_48k_mono.opus
CORPUS_AUDIO_REFERENCE = $(CORPUS_AUDIO:.opus=.s16le.zst)

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

.PHONY: all clean prepare estimate download distclean screenshare screenshare-analyze quality-fixtures verify-quality-fixtures quality-corpus verify-quality-corpus

all: prepare $(INPUT_VIDEO) \
     $(OUT_DIR)/full_f_cbr.h264 \
     $(OUT_DIR)/half_h_cbr.h264 \
     $(OUT_DIR)/quarter_q_cbr.h264

quality-fixtures: $(OUT_DIR)/$(QUALITY_VIDEO) \
                  $(OUT_DIR)/$(QUALITY_VIDEO_REFERENCE) \
                  $(OUT_DIR)/$(QUALITY_AUDIO) \
                  $(OUT_DIR)/$(QUALITY_AUDIO_REFERENCE)

$(OUT_DIR)/$(QUALITY_VIDEO):
	ffmpeg -y -f lavfi -i testsrc2=size=$(QUALITY_WIDTH)x$(QUALITY_HEIGHT):rate=$(QUALITY_FPS) \
		-t $(QUALITY_SECONDS) -pix_fmt yuv420p -c:v libx264 -profile:v baseline -level:v 3.0 \
		-preset ultrafast -tune zerolatency -x264-params keyint=$(QUALITY_FPS):min-keyint=$(QUALITY_FPS):scenecut=0:repeat-headers=1 \
		-f h264 $@

$(OUT_DIR)/$(QUALITY_VIDEO_REFERENCE):
	ffmpeg -y -f lavfi -i testsrc2=size=$(QUALITY_WIDTH)x$(QUALITY_HEIGHT):rate=$(QUALITY_FPS) \
		-t $(QUALITY_SECONDS) -pix_fmt yuv420p -f rawvideo $@

$(OUT_DIR)/$(QUALITY_AUDIO):
	ffmpeg -y -f lavfi -i sine=frequency=997:sample_rate=48000:duration=$(QUALITY_SECONDS) \
		-ac 1 -c:a libopus -application lowdelay -frame_duration 20 -vbr off -b:a 48k -frames:a 150 -f opus $@

$(OUT_DIR)/$(QUALITY_AUDIO_REFERENCE):
	ffmpeg -y -f lavfi -i sine=frequency=997:sample_rate=48000:duration=$(QUALITY_SECONDS) \
		-ac 1 -ar 48000 -f s16le $@

verify-quality-fixtures: quality-fixtures
	@test $$(wc -c < $(OUT_DIR)/$(QUALITY_VIDEO_REFERENCE)) -eq $$(( $(QUALITY_WIDTH) * $(QUALITY_HEIGHT) * 3 / 2 * $(QUALITY_FPS) * $(QUALITY_SECONDS) ))
	@test $$(wc -c < $(OUT_DIR)/$(QUALITY_AUDIO_REFERENCE)) -eq $$(( 48000 * 2 * $(QUALITY_SECONDS) ))

quality-corpus: $(CORPUS_VIDEO) $(CORPUS_VIDEO_REFERENCE) $(CORPUS_AUDIO) $(CORPUS_AUDIO_REFERENCE)

define corpus_video
quality_s$(1)_$(2)p.h264:
	ffmpeg -hide_banner -loglevel error -y -f lavfi -i '$(3)=size=1280x720:rate=$(CORPUS_FPS):duration=$(CORPUS_SECONDS)' \
		-vf "scale=$(4):$(2):flags=lanczos,drawbox=x='mod(t*$(5),iw-$(6))':y='$(7)+mod(t*$(8),ih-$(9))':w=$(6):h=$(10):color=$(11)@1:t=fill,drawbox=x=8:y=8:w=$(12):h=$(13):color=black@1:t=fill,drawtext=fontfile='$(CORPUS_FONT)':text='S$(1) L$(2) E%{eif\\:floor(n/$(CORPUS_EPOCH_FRAMES))\\:d} F%{eif\\:mod(n\\,$(CORPUS_EPOCH_FRAMES))\\:d}':x=14:y=14:fontsize=$(14):fontcolor=white" \
		-frames:v $(CORPUS_FRAMES) -pix_fmt yuv420p -c:v libx264 -profile:v baseline -level:v 3.1 \
		-preset ultrafast -tune zerolatency -b:v $(15)k -maxrate $(15)k -bufsize $(16)k \
		-g $(CORPUS_EPOCH_FRAMES) -keyint_min $(CORPUS_EPOCH_FRAMES) -sc_threshold 0 \
		-x264-params 'repeat-headers=1:aud=1:bframes=0:open-gop=0:threads=1' -f h264 $$@

quality_s$(1)_$(2)p.yuv420p.zst: quality_s$(1)_$(2)p.h264
	ffmpeg -hide_banner -loglevel error -i $$< -pix_fmt yuv420p -f rawvideo - | zstd -19 -T1 -f -o $$@
endef

$(eval $(call corpus_video,0,180,testsrc2,320,390,48,20,210,60,32,white,225,42,18,180,360))
$(eval $(call corpus_video,0,360,testsrc2,640,780,96,40,420,120,64,white,450,84,36,600,1200))
$(eval $(call corpus_video,0,720,testsrc2,1280,1560,192,80,840,240,128,white,900,168,72,1800,3600))
$(eval $(call corpus_video,1,180,smptebars,320,330,48,28,180,64,32,yellow,225,42,18,180,360))
$(eval $(call corpus_video,1,360,smptebars,640,660,96,56,360,128,64,yellow,450,84,36,600,1200))
$(eval $(call corpus_video,1,720,smptebars,1280,1320,192,112,720,256,128,yellow,900,168,72,1800,3600))

quality_a0_48k_mono.opus:
	ffmpeg -hide_banner -loglevel error -f lavfi -i "aevalsrc=if(between(t\\,2\\,3)+between(t\\,5\\,6)\\,0\\,(0.16+0.02*mod(floor(t/0.02)\\,4))*sin(2*PI*(697+73*mod(floor(t/0.02)\\,2))*t)):s=48000:d=$(CORPUS_SECONDS)" -ac 1 -ar 48000 -f s16le - | \
		gst-launch-1.0 -q fdsrc ! rawaudioparse format=pcm pcm-format=s16le sample-rate=48000 num-channels=1 ! \
		opusenc audio-type=voice bitrate=48000 bitrate-type=vbr frame-size=20 dtx=true ! oggmux ! filesink location=$@

quality_a1_48k_mono.opus:
	ffmpeg -hide_banner -loglevel error -f lavfi -i "aevalsrc=if(between(t\\,2\\,3)+between(t\\,5\\,6)\\,0\\,(0.14+0.025*mod(floor(t/0.02)\\,4))*sin(2*PI*(941+268*mod(floor(t/0.02)\\,2))*t)):s=48000:d=$(CORPUS_SECONDS)" -ac 1 -ar 48000 -f s16le - | \
		gst-launch-1.0 -q fdsrc ! rawaudioparse format=pcm pcm-format=s16le sample-rate=48000 num-channels=1 ! \
		opusenc audio-type=voice bitrate=48000 bitrate-type=vbr frame-size=20 dtx=true ! oggmux ! filesink location=$@

quality_a0_48k_mono.s16le.zst: quality_a0_48k_mono.opus
	ffmpeg -hide_banner -loglevel error -i $< -ac 1 -ar 48000 -f s16le - | zstd -19 -T1 -f -o $@

quality_a1_48k_mono.s16le.zst: quality_a1_48k_mono.opus
	ffmpeg -hide_banner -loglevel error -i $< -ac 1 -ar 48000 -f s16le - | zstd -19 -T1 -f -o $@

verify-quality-corpus: quality-corpus
	@for source in 0 1; do \
		for height in 180 360 720; do \
			frames=$$(ffprobe -v error -count_frames -select_streams v:0 -show_entries stream=nb_read_frames -of csv=p=0 quality_s$${source}_$${height}p.h264); \
			test "$$frames" -eq $(CORPUS_FRAMES); \
		done; \
	done
	@for source in 0 1; do \
		packets=$$(ffprobe -v error -count_packets -select_streams a:0 -show_entries stream=nb_read_packets -of csv=p=0 quality_a$${source}_48k_mono.opus); \
		test "$$packets" -eq 301; \
	done

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
$(OUT_DIR)/full_f_cbr.h264:
	ffmpeg -y -f lavfi -i testsrc2=size=1280x720:rate=$(FPS) -t 3 -pix_fmt yuv420p \
		-c:v libx264 -profile:v baseline -level:v 3.1 -preset ultrafast -tune zerolatency \
		-b:v $(BITRATE_F)k -minrate $(BITRATE_F)k -maxrate $(BITRATE_F)k -bufsize $(BITRATE_F)k \
		-g $(FPS) -keyint_min $(FPS) -sc_threshold 0 -x264-params repeat-headers=1:nal-hrd=cbr:force-cfr=1 \
		-f h264 $@

# ==============================================================================
# MID LAYER (360p)
# ==============================================================================
$(OUT_DIR)/half_h_cbr.h264:
	ffmpeg -y -f lavfi -i testsrc2=size=640x360:rate=$(FPS) -t 3 -pix_fmt yuv420p \
		-c:v libx264 -profile:v baseline -level:v 3.1 -preset ultrafast -tune zerolatency \
		-b:v $(BITRATE_H)k -minrate $(BITRATE_H)k -maxrate $(BITRATE_H)k -bufsize $(BITRATE_H)k \
		-g $(FPS) -keyint_min $(FPS) -sc_threshold 0 -x264-params repeat-headers=1:nal-hrd=cbr:force-cfr=1 \
		-f h264 $@

# ==============================================================================
# LOW LAYER (180p)
# ==============================================================================
$(OUT_DIR)/quarter_q_cbr.h264:
	ffmpeg -y -f lavfi -i testsrc2=size=320x180:rate=$(FPS) -t 3 -pix_fmt yuv420p \
		-c:v libx264 -profile:v baseline -level:v 3.0 -preset ultrafast -tune zerolatency \
		-b:v $(BITRATE_Q)k -minrate $(BITRATE_Q)k -maxrate $(BITRATE_Q)k -bufsize $(BITRATE_Q)k \
		-g $(FPS) -keyint_min $(FPS) -sc_threshold 0 -x264-params repeat-headers=1:nal-hrd=cbr:force-cfr=1 \
		-f h264 $@

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
