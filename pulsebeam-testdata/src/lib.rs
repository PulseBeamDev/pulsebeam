pub const RAW_H264_FULL_CBR: &[u8] = include_bytes!("full_f_cbr.h264");
pub const RAW_H264_HALF_CBR: &[u8] = include_bytes!("half_h_cbr.h264");
pub const RAW_H264_QUARTER_CBR: &[u8] = include_bytes!("quarter_q_cbr.h264");

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

        let is_vcl = matches!(nal_type, 1..=5);
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

/// Like `h264_frame_sizes`, but returns the actual byte range of each access
/// unit instead of just its length -- for callers that need to inspect a
/// frame's contents (e.g. classify it as a keyframe) rather than just size it.
pub fn h264_frames(data: &[u8]) -> Vec<&[u8]> {
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

    let mut frames: Vec<&[u8]> = Vec::new();
    let mut frame_start = sc_positions[0];
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

        let is_vcl = matches!(nal_type, 1..=5);
        let starts_new_au = is_vcl && nalu.len() >= 2 && (nalu[1] & 0x80) != 0;

        if starts_new_au && seen_vcl {
            frames.push(&data[frame_start..sc_pos]);
            frame_start = sc_pos;
        }
        if is_vcl {
            seen_vcl = true;
        }
    }
    if frame_start < n {
        frames.push(&data[frame_start..n]);
    }
    frames
}

/// NAL unit type (low 5 bits of the header byte) for every NAL unit found in
/// an Annex-B byte range, in stream order.
///
/// Unlike `h264_frame_sizes`, this does not group NALs into access units — it
/// is meant to be run on a single already-reassembled access unit (e.g. one
/// depacketized `MediaFrame`) to classify it, so callers only need to know
/// whether a VCL slice (type 1 = non-IDR, type 5 = IDR) is present.
pub fn h264_nal_types(data: &[u8]) -> Vec<u8> {
    let n = data.len();
    let mut types = Vec::new();
    let mut i = 0;
    while i + 2 < n {
        let header_start = if i + 3 < n
            && data[i] == 0
            && data[i + 1] == 0
            && data[i + 2] == 0
            && data[i + 3] == 1
        {
            i += 4;
            i
        } else if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            i += 3;
            i
        } else {
            i += 1;
            continue;
        };
        if let Some(&header) = data.get(header_start) {
            types.push(header & 0x1f);
        }
    }
    types
}

/// Whether a reassembled access unit contains an IDR slice (NAL type 5).
///
/// A decoder can only safely start, or resume after a mid-GOP splice, at an
/// IDR: any other NAL type refers to reference pictures that may not exist
/// in the decoder's buffer.
pub fn h264_frame_is_keyframe(data: &[u8]) -> bool {
    h264_nal_types(data).contains(&5)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264_frames_agrees_with_h264_frame_sizes_on_frame_count_and_coverage() {
        // `h264_frame_sizes` counts only NALU payload bytes (no start codes),
        // while `h264_frames` returns the raw byte range including each
        // NALU's start code -- so lengths differ by a few bytes per NALU,
        // but both must agree on how many access units the stream contains,
        // and `h264_frames` must partition the buffer contiguously with no
        // gaps or overlaps.
        for data in [RAW_H264_FULL_CBR, RAW_H264_HALF_CBR, RAW_H264_QUARTER_CBR] {
            let sizes = h264_frame_sizes(data);
            let frames = h264_frames(data);
            assert_eq!(sizes.len(), frames.len());

            let base = data.as_ptr() as usize;
            let offset_of = |frame: &[u8]| frame.as_ptr() as usize - base;
            let mut cursor = offset_of(frames[0]);
            for frame in &frames {
                assert_eq!(offset_of(frame), cursor, "frames must be contiguous");
                cursor += frame.len();
            }
            assert_eq!(cursor, data.len());
        }
    }

    #[test]
    fn each_test_asset_has_exactly_one_keyframe_per_loop() {
        for data in [RAW_H264_FULL_CBR, RAW_H264_HALF_CBR, RAW_H264_QUARTER_CBR] {
            let keyframe_count = h264_frames(data)
                .iter()
                .filter(|frame| h264_frame_is_keyframe(frame))
                .count();
            assert_eq!(keyframe_count, 1);
        }
    }

    #[test]
    fn first_frame_of_every_test_asset_is_a_keyframe() {
        for data in [RAW_H264_FULL_CBR, RAW_H264_HALF_CBR, RAW_H264_QUARTER_CBR] {
            let frames = h264_frames(data);
            assert!(h264_frame_is_keyframe(frames[0]));
        }
    }
}
