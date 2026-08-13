//! Minimal streaming MD5 (RFC 1321) for the FLAC STREAMINFO audio signature.
//!
//! Self-contained — no dependency, no FFI — and validated against the RFC test
//! vectors. FLAC hashes the *unencoded* interleaved little-endian samples so a
//! decoder can verify the audio survived intact (`flac -t`), independent of
//! whether the bitstream itself parsed.

/// Constants K[i] = floor(2^32 · |sin(i+1)|). (Rotation amounts are literal
/// in the unrolled rounds.)
#[rustfmt::skip]
const K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

pub struct Md5 {
    state: [u32; 4],
    buf: [u8; 64],
    buflen: usize,
    total: u64, // total bytes fed
}

impl Md5 {
    pub fn new() -> Self {
        Md5 {
            state: [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476],
            buf: [0; 64],
            buflen: 0,
            total: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        if self.buflen > 0 {
            let take = (64 - self.buflen).min(data.len());
            self.buf[self.buflen..self.buflen + take].copy_from_slice(&data[..take]);
            self.buflen += take;
            data = &data[take..];
            if self.buflen == 64 {
                let block = self.buf;
                self.process(&block);
                self.buflen = 0;
            }
        }
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            self.process(&block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buflen = data.len();
        }
    }

    pub fn finalize(mut self) -> [u8; 16] {
        let bit_len = self.total.wrapping_mul(8);
        let mut buf = self.buf;
        let mut len = self.buflen;
        buf[len] = 0x80;
        len += 1;
        if len > 56 {
            for b in &mut buf[len..64] {
                *b = 0;
            }
            let block = buf;
            self.process(&block);
            buf = [0; 64];
            len = 0;
        }
        for b in &mut buf[len..56] {
            *b = 0;
        }
        buf[56..64].copy_from_slice(&bit_len.to_le_bytes());
        let block = buf;
        self.process(&block);

        let mut out = [0u8; 16];
        for (i, s) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&s.to_le_bytes());
        }
        out
    }

    fn process(&mut self, block: &[u8; 64]) {
        let mut m = [0u32; 16];
        for (i, w) in m.iter_mut().enumerate() {
            *w = u32::from_le_bytes(block[i * 4..i * 4 + 4].try_into().unwrap());
        }
        let [mut a, mut b, mut c, mut d] = self.state;

        // Fully unrolled rounds: constant indices/rotations resolve at compile
        // time (the branchy 0..64 loop cost ~30% more per block).
        macro_rules! step {
            ($f:expr, $a:ident, $b:ident, $c:ident, $d:ident, $i:expr, $g:expr, $s:expr) => {
                let f = ($f)
                    .wrapping_add($a)
                    .wrapping_add(K[$i])
                    .wrapping_add(m[$g]);
                $a = $b.wrapping_add(f.rotate_left($s));
            };
        }
        macro_rules! round1 {
            ($a:ident, $b:ident, $c:ident, $d:ident, $i:expr, $s:expr) => {
                step!(($b & $c) | (!$b & $d), $a, $b, $c, $d, $i, $i, $s);
            };
        }
        macro_rules! round2 {
            ($a:ident, $b:ident, $c:ident, $d:ident, $i:expr, $s:expr) => {
                step!(
                    ($d & $b) | (!$d & $c),
                    $a,
                    $b,
                    $c,
                    $d,
                    $i,
                    (5 * $i + 1) % 16,
                    $s
                );
            };
        }
        macro_rules! round3 {
            ($a:ident, $b:ident, $c:ident, $d:ident, $i:expr, $s:expr) => {
                step!($b ^ $c ^ $d, $a, $b, $c, $d, $i, (3 * $i + 5) % 16, $s);
            };
        }
        macro_rules! round4 {
            ($a:ident, $b:ident, $c:ident, $d:ident, $i:expr, $s:expr) => {
                step!($c ^ ($b | !$d), $a, $b, $c, $d, $i, (7 * $i) % 16, $s);
            };
        }
        macro_rules! quad {
            ($round:ident, $base:expr, $s0:expr, $s1:expr, $s2:expr, $s3:expr) => {
                $round!(a, b, c, d, $base, $s0);
                $round!(d, a, b, c, $base + 1, $s1);
                $round!(c, d, a, b, $base + 2, $s2);
                $round!(b, c, d, a, $base + 3, $s3);
            };
        }
        quad!(round1, 0, 7, 12, 17, 22);
        quad!(round1, 4, 7, 12, 17, 22);
        quad!(round1, 8, 7, 12, 17, 22);
        quad!(round1, 12, 7, 12, 17, 22);
        quad!(round2, 16, 5, 9, 14, 20);
        quad!(round2, 20, 5, 9, 14, 20);
        quad!(round2, 24, 5, 9, 14, 20);
        quad!(round2, 28, 5, 9, 14, 20);
        quad!(round3, 32, 4, 11, 16, 23);
        quad!(round3, 36, 4, 11, 16, 23);
        quad!(round3, 40, 4, 11, 16, 23);
        quad!(round3, 44, 4, 11, 16, 23);
        quad!(round4, 48, 6, 10, 15, 21);
        quad!(round4, 52, 6, 10, 15, 21);
        quad!(round4, 56, 6, 10, 15, 21);
        quad!(round4, 60, 6, 10, 15, 21);

        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
    }
}

#[cfg(test)]
mod tests {
    use super::Md5;

    fn hex(bytes: [u8; 16]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn rfc1321_vectors() {
        let mut m = Md5::new();
        m.update(b"");
        assert_eq!(hex(m.finalize()), "d41d8cd98f00b204e9800998ecf8427e");

        let mut m = Md5::new();
        m.update(b"abc");
        assert_eq!(hex(m.finalize()), "900150983cd24fb0d6963f7d28e17f72");

        let mut m = Md5::new();
        m.update(b"The quick brown fox jumps over the lazy dog");
        assert_eq!(hex(m.finalize()), "9e107d9d372bb6826bd81d3542a419d6");
    }

    /// Streaming in odd-sized chunks must equal a single update.
    #[test]
    fn chunked_equals_whole() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7) as u8).collect();
        let mut whole = Md5::new();
        whole.update(&data);
        let whole = whole.finalize();

        let mut chunked = Md5::new();
        for c in data.chunks(7) {
            chunked.update(c);
        }
        assert_eq!(chunked.finalize(), whole);
    }
}
