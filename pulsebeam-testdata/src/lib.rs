pub const RAW_H264_FULL: &[u8] = include_bytes!("full_f.h264");
pub const RAW_H264_HALF: &[u8] = include_bytes!("half_h.h264");
pub const RAW_H264_QUARTER: &[u8] = include_bytes!("quarter_q.h264");

// 16 video and 5 audio downstream slots
pub const RAW_CHROME_SDP: &str = include_str!("chrome.sdp");

/// Parse an Annex-B H.264 byte-stream and return **per-frame** (per access
/// unit) byte sizes.
///
/// A new access unit starts when a VCL NAL unit (type 1 or 5) whose
/// `first_mb_in_slice == 0` is encountered.  In CBR-HRD streams (`nal-hrd=cbr`)
/// x264 emits multiple slice NALUs per frame plus SEI / filler padding; grouping
/// them correctly keeps the simulated bytes-per-second equal to the actual
/// encoded bitrate.
///
/// `first_mb_in_slice == 0` ↔ MSB of the first payload byte is `1`
/// (Exp-Golomb code-word for 0 is the single bit `1`).
pub fn h264_frame_sizes(data: &[u8]) -> Vec<usize> {
    let n = data.len();
    let mut sc_positions: Vec<usize> = Vec::new();
    let mut i = 0;
    while i + 2 < n {
        if data[i] == 0 && data[i + 1] == 0 {
            if i + 3 < n && data[i + 2] == 0 && data[i + 3] == 1 {
                sc_positions.push(i);
                i += 4;
                continue;
            }
            if data[i + 2] == 1 {
                sc_positions.push(i);
                i += 3;
                continue;
            }
        }
        i += 1;
    }
    if sc_positions.is_empty() {
        return vec![];
    }

    let mut frames: Vec<usize> = Vec::new();
    let mut current_frame_bytes: usize = 0;
    let mut seen_vcl = false;

    for (k, &sc_pos) in sc_positions.iter().enumerate() {
        let sc_len = if sc_pos + 3 < n && data[sc_pos + 2] == 0 {
            4
        } else {
            3
        };
        let nalu_start = sc_pos + sc_len;
        let nalu_end = if k + 1 < sc_positions.len() {
            sc_positions[k + 1]
        } else {
            n
        };
        if nalu_start >= nalu_end {
            continue;
        }
        let nalu = &data[nalu_start..nalu_end];
        let nal_type = nalu[0] & 0x1f;
        let nalu_size = nalu_end - nalu_start;

        let is_vcl = matches!(nal_type, 1 | 2 | 3 | 4 | 5);
        // first_mb_in_slice == 0  ↔  MSB of byte[1] set (Exp-Golomb "1" prefix).
        let starts_new_au = is_vcl && nalu.len() >= 2 && (nalu[1] & 0x80) != 0;

        if starts_new_au && seen_vcl {
            frames.push(current_frame_bytes);
            current_frame_bytes = 0;
        }
        if is_vcl {
            seen_vcl = true;
        }
        current_frame_bytes += nalu_size;
    }
    if current_frame_bytes > 0 {
        frames.push(current_frame_bytes);
    }
    frames
}
