pub struct H264FrameSlicer<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> H264FrameSlicer<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    fn next_nalu(&self, start: usize) -> Option<(usize, usize, u8)> {
        let mut position = start;
        while position.saturating_add(3) < self.data.len() {
            let Some(header) = start_code_at(self.data, position) else {
                position = position.saturating_add(1);
                continue;
            };
            let nalu_start = position;
            let nalu_type = self.data.get(header).map_or(0, |byte| byte & 0x1f);
            let mut next = header;
            while next.saturating_add(3) < self.data.len() {
                if start_code_at(self.data, next).is_some() {
                    return Some((nalu_start, next, nalu_type));
                }
                next = next.saturating_add(1);
            }
            return Some((nalu_start, self.data.len(), nalu_type));
        }
        None
    }

    fn starts_access_unit(&self, nalu_type: u8, nalu_start: usize) -> bool {
        match nalu_type {
            6..=9 => true,
            1 | 5 => {
                let header = start_code_at(self.data, nalu_start)
                    .unwrap_or_else(|| nalu_start.saturating_add(3));
                self.data
                    .get(header.saturating_add(1))
                    .is_some_and(|first_slice_byte| first_slice_byte & 0x80 != 0)
            }
            _ => false,
        }
    }
}

impl<'a> Iterator for H264FrameSlicer<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.position >= self.data.len() {
            return None;
        }
        let frame_start = self.position;
        let mut frame_end = self.position;
        let mut has_slice = false;
        let mut search = self.position;
        while let Some((nalu_start, nalu_end, nalu_type)) = self.next_nalu(search) {
            if has_slice && self.starts_access_unit(nalu_type, nalu_start) {
                self.position = nalu_start;
                return self.data.get(frame_start..nalu_start);
            }
            if matches!(nalu_type, 1 | 5) {
                has_slice = true;
            }
            frame_end = nalu_end;
            search = nalu_end;
        }
        self.position = self.data.len();
        (frame_end > frame_start)
            .then(|| self.data.get(frame_start..frame_end))
            .flatten()
    }
}

fn start_code_at(data: &[u8], position: usize) -> Option<usize> {
    if data.get(position) != Some(&0) || data.get(position.saturating_add(1)) != Some(&0) {
        return None;
    }
    match data.get(position.saturating_add(2)) {
        Some(&1) => Some(position.saturating_add(3)),
        Some(&0) if data.get(position.saturating_add(3)) == Some(&1) => {
            Some(position.saturating_add(4))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::H264FrameSlicer;

    #[test]
    fn annex_b_stream_is_split_at_access_unit_boundaries() {
        let stream = [
            0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1f, 0, 0, 1, 0x68, 0xce, 0x06, 0, 0, 0, 1, 0x65, 0x80,
            0x11, 0, 0, 1, 0x41, 0x80, 0x22,
        ];

        let frames = H264FrameSlicer::new(&stream).collect::<Vec<_>>();

        assert_eq!(frames.len(), 2);
        assert!(frames[0].windows(2).any(|bytes| bytes == [0x65, 0x80]));
        assert!(frames[1].windows(2).any(|bytes| bytes == [0x41, 0x80]));
    }
}
