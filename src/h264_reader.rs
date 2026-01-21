/// Reassembler for NAL units
pub(crate) struct NalReassembler {
    buffer: Vec<u8>,
}

impl NalReassembler {
    pub(crate) fn new() -> Self {
        NalReassembler { buffer: Vec::new() }
    }

    /// Feed new H.264 payload and extract complete NALs
    pub(crate) fn feed(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.buffer.extend_from_slice(data);
        let mut nals = Vec::new();
        let mut i = 0;

        while i + 3 < self.buffer.len() {
            // Check for 4-byte start code
            if self.buffer[i..].starts_with(&[0x00, 0x00, 0x00, 0x01]) {
                if i != 0 {
                    // Slice previous NAL
                    let nal = self.buffer[..i].to_vec();
                    nals.push(nal);
                    self.buffer.drain(..i); // remove output NAL
                    i = 0;
                    continue;
                }
                i += 4;
            } else {
                i += 1;
            }
        }

        nals
    }

    /// Flush remaining partial NAL
    pub(crate) fn flush(&mut self) -> Option<Vec<u8>> {
        if !self.buffer.is_empty() {
            Some(self.buffer.split_off(0))
        } else {
            None
        }
    }
}