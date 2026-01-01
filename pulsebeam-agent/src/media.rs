use bytes::Bytes;

pub struct H264Looper {
    frames: Vec<Bytes>,
    index: usize,
}

impl H264Looper {
    pub fn new(data: &[u8]) -> Self {
        let slicer = H264FrameSlicer::new(data);
        let frames: Vec<Bytes> = slicer.map(Bytes::copy_from_slice).collect();
        tracing::info!(
            "H264Looper: found {} complete frames (Access Units)",
            frames.len()
        );
        Self { frames, index: 0 }
    }
}

impl Iterator for H264Looper {
    type Item = Bytes;

    fn next(&mut self) -> Option<Self::Item> {
        if self.frames.is_empty() {
            return None;
        }

        let frame = &self.frames[self.index];
        self.index = (self.index + 1) % self.frames.len();
        Some(frame.clone())
    }
}

pub struct H264FrameSlicer<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> H264FrameSlicer<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Finds the boundaries of the next NALU in the Annex B stream.
    fn next_nalu_bounds(&self, start: usize) -> Option<(usize, usize, u8)> {
        let mut i = start;
        while i + 3 < self.data.len() {
            // Check for start code 00 00 01 or 00 00 00 01
            if self.data[i] == 0
                && self.data[i + 1] == 0
                && (self.data[i + 2] == 1 || (self.data[i + 2] == 0 && self.data[i + 3] == 1))
            {
                let nalu_start = i;
                let header_pos = if self.data[i + 2] == 1 { i + 3 } else { i + 4 };
                let nalu_type = self.data[header_pos] & 0x1F;

                // Find the start of the NEXT NALU to determine the end of this one
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

    /// Determines if a NALU is the start of a brand new Access Unit (Frame).
    fn is_new_access_unit(&self, nalu_type: u8, nalu_start: usize, _nalu_end: usize) -> bool {
        match nalu_type {
            // AUD (9), SPS (7), PPS (8), SEI (6)
            // These always precede the VCL slices of a new frame.
            6..=9 => true,
            // IDR (5) or Non-IDR (1) slices
            1 | 5 => {
                // To be precise, we check if first_mb_in_slice == 0.
                // It is an Exp-Golomb encoded value. If the first bit of the
                // slice header payload is 1, the value is 0.
                let header_pos = if self.data[nalu_start + 2] == 1 {
                    nalu_start + 3
                } else {
                    nalu_start + 4
                };

                if self.data.len() > header_pos + 1 {
                    let first_byte_of_slice_header = self.data[header_pos + 1];
                    // If high bit is 1, first_mb_in_slice is 0 -> New Frame
                    return (first_byte_of_slice_header & 0x80) != 0;
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

        let start_pos = self.pos;
        let mut end_pos = self.pos;
        let mut has_vcl = false;

        // We iterate through NALUs until we find one that belongs to the NEXT frame
        let mut search_pos = self.pos;
        while let Some((n_start, n_end, n_type)) = self.next_nalu_bounds(search_pos) {
            // If we already have some VCL (slice) data and we hit a NALU that
            // signals a new frame, we stop and return everything up to this point.
            if has_vcl && self.is_new_access_unit(n_type, n_start, n_end) {
                self.pos = n_start;
                return Some(&self.data[start_pos..n_start]);
            }

            if n_type == 1 || n_type == 5 {
                has_vcl = true;
            }

            end_pos = n_end;
            search_pos = n_end;
        }

        // Handle the last frame in the buffer
        self.pos = self.data.len();
        if end_pos > start_pos {
            Some(&self.data[start_pos..end_pos])
        } else {
            None
        }
    }
}
