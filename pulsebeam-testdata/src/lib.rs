pub const RAW_H264_FULL_CBR: &[u8] = include_bytes!("full_f_cbr.h264");
pub const RAW_H264_HALF_CBR: &[u8] = include_bytes!("half_h_cbr.h264");
pub const RAW_H264_QUARTER_CBR: &[u8] = include_bytes!("quarter_q_cbr.h264");
pub const RAW_H264_SCREEN_FULL_VBR: &[u8] = include_bytes!("screen_f_vbr.h264");
pub const RAW_H264_SCREEN_FULL_TIMING: &str = include_str!("screen_f_vbr.timing");

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

pub fn frame_timestamps_micros(data: &str) -> Vec<u64> {
    let timestamps: Vec<u64> = data
        .lines()
        .map(|line| line.parse().expect("valid frame timestamp"))
        .collect();
    debug_assert!(!timestamps.is_empty());
    debug_assert!(timestamps.windows(2).all(|pair| pair[0] < pair[1]));
    timestamps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_share_fixtures_have_variable_cadence_and_low_static_bitrate() {
        let sizes = h264_frame_sizes(RAW_H264_SCREEN_FULL_VBR);
        let timestamps = frame_timestamps_micros(RAW_H264_SCREEN_FULL_TIMING);
        assert_eq!(sizes.len(), timestamps.len(), "f frame schedule");

        let gaps: Vec<u64> = timestamps
            .windows(2)
            .map(|pair| pair[1] - pair[0])
            .collect();
        assert!(
            gaps.iter().any(|gap| *gap <= 67_000),
            "f has 15fps motion cadence"
        );
        assert!(
            gaps.iter().any(|gap| *gap >= 2_000_000),
            "f has 0.5fps static cadence"
        );

        let mut static_window_rates = Vec::new();
        for second in 0..timestamps.last().copied().unwrap() / 1_000_000 {
            let indices: Vec<usize> = timestamps
                .iter()
                .enumerate()
                .filter(|(_, timestamp)| **timestamp / 1_000_000 == second)
                .map(|(index, _)| index)
                .collect();
            if indices.len() <= 1 {
                static_window_rates.push(
                    indices
                        .iter()
                        .map(|index| sizes[*index] as u64)
                        .sum::<u64>()
                        * 8
                        / 1_000,
                );
            }
        }
        assert!(!static_window_rates.is_empty(), "f has static windows");
        static_window_rates.sort_unstable();
        let median = static_window_rates[static_window_rates.len() / 2];
        assert!(median <= 20, "f static median {median}kbps exceeds 20kbps");
    }
}
