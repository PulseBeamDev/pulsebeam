prepare:
	ffmpeg -r 30 -t 30 -i video.mp4 \
    -filter_complex "[0:v]split=3[v_f][v_h][v_q]; \
                     [v_h]scale=-2:360[h_scaled]; \
                     [v_q]scale=-2:180[q_scaled]" \
    -map "[v_f]" -c:v:0 libx264 -profile:v:0 baseline -b:v:0 1250k -minrate:v:0 1250k -maxrate:v:0 1250k -bufsize:v:0 1250k \
    -g 60 -tune zerolatency -x264-params "annexb=1:repeat-headers=1:nal-hrd=cbr:filler=1" -bf 0 -r 30 full_f.h264 \
    \
    -map "[h_scaled]" -c:v:1 libx264 -profile:v:1 baseline -b:v:1 400k -minrate:v:1 400k -maxrate:v:1 400k -bufsize:v:1 400k \
    -g 60 -tune zerolatency -x264-params "annexb=1:repeat-headers=1:nal-hrd=cbr:filler=1" -bf 0 -r 30 half_h.h264 \
    \
    -map "[q_scaled]" -c:v:2 libx264 -profile:v:2 baseline -b:v:2 150k -minrate:v:2 150k -maxrate:v:2 150k -bufsize:v:2 150k \
    -g 60 -tune zerolatency -x264-params "annexb=1:repeat-headers=1:nal-hrd=cbr:filler=1" -bf 0 -r 30 quarter_q.h264
