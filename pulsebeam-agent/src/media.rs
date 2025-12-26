pub struct H264FrameSlicer<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> H264FrameSlicer<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Hardened NAL detection: returns (start_pos, end_pos, nalu_type)
    fn next_nalu_bounds(&self, start: usize) -> Option<(usize, usize, u8)> {
        let mut i = start;
        // Find start code
        while i + 3 < self.data.len() {
            if self.data[i] == 0
                && self.data[i + 1] == 0
                && (self.data[i + 2] == 1 || (self.data[i + 2] == 0 && self.data[i + 3] == 1))
            {
                let nalu_start = i;
                let header_pos = if self.data[i + 2] == 1 { i + 3 } else { i + 4 };
                let nalu_type = self.data[header_pos] & 0x1F;

                // Find next start code to get the end
                let mut next = header_pos;
                while next + 3 < self.data.len() {
                    if self.data[next] == 0
                        && self.data[next + 1] == 0
                        && (self.data[next + 2] == 1
                            || (self.data[next + 2] == 0 && self.data[next + 3] == 1))
                    {
                        return Some((nalu_start, next, nalu_type));
                    }
                    next += 1;
                }
                return Some((nalu_start, self.data.len(), nalu_type));
            }
            i += 1;
        }
        None
    }

    /// Production peeking: Does this NAL start a new frame?
    fn starts_new_frame(nalu_type: u8, nalu_data: &[u8]) -> bool {
        match nalu_type {
            // SPS, PPS, AUD, and SEI always start/precede a new AU
            6..=9 => true,
            // Slices (1 = Non-IDR, 5 = IDR)
            1 | 5 => {
                // Peek at slice header for first_mb_in_slice
                // In Annex B, the header is at index 3 or 4.
                // We find the first non-zero/one byte.
                if let Some(pos) = nalu_data.iter().position(|&b| b != 0 && b != 1) {
                    if nalu_data.len() > pos + 1 {
                        let first_byte_of_payload = nalu_data[pos + 1];
                        // Exp-Golomb for '0' is just the bit '1'.
                        // If the first bit of the payload is 1, first_mb_in_slice is 0.
                        return (first_byte_of_payload & 0x80) != 0;
                    }
                }
                false
            }
            _ => false,
        }
    }
}

impl<'a> Iterator for H264FrameSlicer<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.data.len() {
            return None;
        }

        let mut end_pos = self.pos;
        let mut first_nalu = true;

        while let Some((n_start, n_end, n_type)) = self.next_nalu_bounds(end_pos) {
            // If this NAL starts a new frame and isn't the first one we're looking at,
            // then the PREVIOUS NALUs form the complete frame.
            if !first_nalu && Self::starts_new_frame(n_type, &self.data[n_start..n_end]) {
                let frame = &self.data[self.pos..n_start];
                self.pos = n_start;
                return Some(frame);
            }

            end_pos = n_end;
            first_nalu = false;
            if n_end == self.data.len() {
                break;
            }
        }

        let frame = &self.data[self.pos..];
        self.pos = self.data.len();
        Some(frame)
    }
}
