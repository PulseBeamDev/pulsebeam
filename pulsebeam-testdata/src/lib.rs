pub const RAW_H264_FULL: &[u8] = include_bytes!("full_f.h264");
pub const RAW_H264_HALF: &[u8] = include_bytes!("half_h.h264");
pub const RAW_H264_QUARTER: &[u8] = include_bytes!("quarter_q.h264");

// 16 video and 5 audio downstream slots
pub const RAW_CHROME_SDP: &str = include_str!("chrome.sdp");
