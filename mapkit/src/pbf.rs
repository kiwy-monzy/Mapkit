//! Minimal protobuf reader — just enough for MVT (Mapbox Vector Tiles).

pub struct Reader<'a> {
    pub b: &'a [u8],
    pub i: usize,
}

impl<'a> Reader<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Reader { b, i: 0 }
    }

    pub fn eof(&self) -> bool {
        self.i >= self.b.len()
    }

    pub fn varint(&mut self) -> u64 {
        let mut v: u64 = 0;
        let mut shift = 0u32;
        while self.i < self.b.len() {
            let c = self.b[self.i];
            self.i += 1;
            v |= ((c & 0x7f) as u64) << shift;
            if c & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        v
    }

    /// Returns (field_number, wire_type) or None at end.
    pub fn key(&mut self) -> Option<(u32, u32)> {
        if self.eof() {
            return None;
        }
        let k = self.varint();
        Some(((k >> 3) as u32, (k & 7) as u32))
    }

    pub fn len_varint(&mut self) -> usize {
        self.varint() as usize
    }

    pub fn bytes(&mut self, n: usize) -> &'a [u8] {
        let s = self.i.min(self.b.len());
        let e = (s + n).min(self.b.len());
        self.i = e;
        &self.b[s..e]
    }

    pub fn skip(&mut self, wire: u32) {
        match wire {
            0 => {
                self.varint();
            }
            1 => {
                self.i += 8;
            }
            2 => {
                let n = self.len_varint();
                self.i += n;
            }
            5 => {
                self.i += 4;
            }
            _ => {
                self.i += 1;
            }
        }
    }
}

pub fn zigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}
