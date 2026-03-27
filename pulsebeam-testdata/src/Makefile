# Configurable variables
INPUT_VIDEO = video.mp4
OUT_DIR = .
FPS = 30
GOP = 60 # 2 second GOP at 30fps

# Bitrates (bps)
BITRATE_F = 1250k
BITRATE_H = 400k
BITRATE_Q = 150k

# x264 parameters for WebRTC realism: 
# - filler=1: forces CBR with padding
# - repeat-headers=1: puts SPS/PPS before every IDR frame
# - nal-hrd=cbr: essential for strict CBR
X264_OPTS = "annexb=1:repeat-headers=1:nal-hrd=cbr:filler=1:force-cfr=1"

.PHONY: all clean prepare

all: prepare $(OUT_DIR)/full_f.h264 $(OUT_DIR)/half_h.h264 $(OUT_DIR)/quarter_q.h264

prepare:
	mkdir -p $(OUT_DIR)

$(OUT_DIR)/full_f.h264 $(OUT_DIR)/half_h.h264 $(OUT_DIR)/quarter_q.h264: $(INPUT_VIDEO)
	ffmpeg -i $(INPUT_VIDEO) -t 30 -y \
		-filter_complex "[0:v]fps=$(FPS),split=3[v_f][v_h][v_q]; \
						 [v_h]scale=-2:360[h_scaled]; \
						 [v_q]scale=-2:180[q_scaled]" \
		-map "[v_f]" -c:v:0 libx264 -profile:v:0 baseline -preset ultrafast \
			-b:v:0 $(BITRATE_F) -minrate:v:0 $(BITRATE_F) -maxrate:v:0 $(BITRATE_F) -bufsize:v:0 $(BITRATE_F) \
			-g $(GOP) -keyint_min $(GOP) -sc_threshold 0 -tune zerolatency \
			-x264-params $(X264_OPTS) -bf 0 $(OUT_DIR)/full_f.h264 \
		-map "[h_scaled]" -c:v:1 libx264 -profile:v:1 baseline -preset ultrafast \
			-b:v:1 $(BITRATE_H) -minrate:v:1 $(BITRATE_H) -maxrate:v:1 $(BITRATE_H) -bufsize:v:1 $(BITRATE_H) \
			-g $(GOP) -keyint_min $(GOP) -sc_threshold 0 -tune zerolatency \
			-x264-params $(X264_OPTS) -bf 0 $(OUT_DIR)/half_h.h264 \
		-map "[q_scaled]" -c:v:2 libx264 -profile:v:2 baseline -preset ultrafast \
			-b:v:2 $(BITRATE_Q) -minrate:v:2 $(BITRATE_Q) -maxrate:v:2 $(BITRATE_Q) -bufsize:v:2 $(BITRATE_Q) \
			-g $(GOP) -keyint_min $(GOP) -sc_threshold 0 -tune zerolatency \
			-x264-params $(X264_OPTS) -bf 0 $(OUT_DIR)/quarter_q.h264

clean:
	rm -rf $(OUT_DIR)
