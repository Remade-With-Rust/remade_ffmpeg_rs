use crate::decode::error::{Error, Result};
use crate::decode::marker::Marker;
use crate::decode::parser::ScanInfo;
use crate::decode::read_u8;
use alloc::borrow::ToOwned;
use alloc::vec;
use alloc::vec::Vec;
use std::io::Read;

const LUT_BITS: u8 = 8;

#[derive(Debug)]
pub struct HuffmanDecoder {
    bits: u64,
    num_bits: u8,
    marker: Option<Marker>,
}

impl HuffmanDecoder {
    pub fn new() -> HuffmanDecoder {
        HuffmanDecoder {
            bits: 0,
            num_bits: 0,
            marker: None,
        }
    }

    // Section F.2.2.3
    // Figure F.16
    pub fn decode<R: Read>(
        &mut self,
        reader: &mut crate::decode::Buffered<R>,
        table: &HuffmanTable,
    ) -> Result<u8> {
        crate::prof::bump(crate::prof::Count::DecSymbols, 1);
        if self.num_bits < 16 {
            self.read_bits(reader)?;
        }

        let (value, size) = table.lut[self.peek_bits(LUT_BITS) as usize & ((1 << LUT_BITS) - 1)];

        if size > 0 {
            crate::prof::bump(crate::prof::Count::DecLutHit, 1);
            self.consume_bits(size);
            Ok(value)
        } else {
            crate::prof::bump(crate::prof::Count::DecLutMiss, 1);
            let bits = self.peek_bits(16);

            for i in LUT_BITS..16 {
                let code = (bits >> (15 - i)) as i32;

                if code <= table.maxcode[i as usize] {
                    self.consume_bits(i + 1);

                    let index = (code + table.delta[i as usize]) as usize;
                    return Ok(table.values[index]);
                }
            }

            Err(Error::Format("failed to decode huffman code".to_owned()))
        }
    }

    pub fn decode_fast_ac<R: Read>(
        &mut self,
        reader: &mut crate::decode::Buffered<R>,
        table: &HuffmanTable,
    ) -> Result<Option<(i16, u8)>> {
        if self.num_bits < LUT_BITS {
            self.read_bits(reader)?;
        }

        let (value, run_size) =
            table.ac_lut[self.peek_bits(LUT_BITS) as usize & ((1 << LUT_BITS) - 1)];

        if run_size != 0 {
            crate::prof::bump(crate::prof::Count::DecFastAcHit, 1);
            let run = run_size >> 4;
            let size = run_size & 0x0f;

            self.consume_bits(size);
            return Ok(Some((value, run)));
        }

        crate::prof::bump(crate::prof::Count::DecFastAcMiss, 1);
        Ok(None)
    }

    #[inline]
    pub fn get_bits<R: Read>(
        &mut self,
        reader: &mut crate::decode::Buffered<R>,
        count: u8,
    ) -> Result<u16> {
        if self.num_bits < count {
            self.read_bits(reader)?;
        }

        let bits = self.peek_bits(count);
        self.consume_bits(count);

        Ok(bits)
    }

    #[inline]
    pub fn receive_extend<R: Read>(
        &mut self,
        reader: &mut crate::decode::Buffered<R>,
        count: u8,
    ) -> Result<i16> {
        crate::prof::bump(crate::prof::Count::DecReceiveExtend, 1);
        let value = self.get_bits(reader, count)?;
        Ok(extend(value, count))
    }

    pub fn reset(&mut self) {
        self.bits = 0;
        self.num_bits = 0;
    }

    pub fn take_marker<R: Read>(
        &mut self,
        reader: &mut crate::decode::Buffered<R>,
    ) -> Result<Option<Marker>> {
        self.read_bits(reader).map(|_| self.marker.take())
    }

    #[inline]
    fn peek_bits(&mut self, count: u8) -> u16 {
        debug_assert!(count <= 16);
        debug_assert!(self.num_bits >= count);

        ((self.bits >> (64 - count)) & ((1 << count) - 1)) as u16
    }

    #[inline]
    fn consume_bits(&mut self, count: u8) {
        debug_assert!(self.num_bits >= count);

        self.bits <<= count as usize;
        self.num_bits -= count;
    }

    fn read_bits<R: Read>(&mut self, reader: &mut crate::decode::Buffered<R>) -> Result<()> {
        crate::prof::bump(crate::prof::Count::DecRefills, 1);
        // Hoisted out of the byte loop: this is a measurement knob, and reading
        // it per byte would put an atomic load on the hottest path in the
        // decoder (260k bytes/frame) to serve a switch that never changes.
        let slow = crate::decode::ablate_slow_bytes();

        // Bulk refill: absorb every byte the accumulator has room for in ONE
        // step, when the buffer holds them and none is 0xFF.
        //
        // The byte loop below places byte i at bit `56 - num_bits - 8i`, i.e. it
        // packs them big-endian from the top down — which is exactly
        // `u64::from_be_bytes(window) >> num_bits`. Counters said 6.8 bytes per
        // refill, so this replaces ~7 iterations (each with a branch, a shift
        // and an 0xFF test) with one load, one scan and one shift.
        //
        // The 0xFF test cannot be skipped: a 0xFF inside the entropy stream is
        // either byte-stuffing or a marker, and both need the slow path. Bailing
        // to it costs nothing because nothing has been consumed yet.
        if !slow && self.marker.is_none() {
            let want = ((64 - self.num_bits) / 8) as usize;
            let avail = reader.buffered();
            if want > 0 && avail.len() >= want && !avail[..want].contains(&0xFF) {
                let mut window = [0u8; 8];
                window[..want].copy_from_slice(&avail[..want]);
                self.bits |= u64::from_be_bytes(window) >> self.num_bits;
                self.num_bits += (want * 8) as u8;
                reader.consume(want);
                crate::prof::bump(crate::prof::Count::DecBulkRefills, 1);
                return Ok(());
            }
        }

        while self.num_bits <= 56 {
            crate::prof::bump(crate::prof::Count::DecBytesRead, 1);
            // Fill with zero bits if we have reached the end.
            let byte = match self.marker {
                Some(_) => 0,
                None if slow => read_u8(reader)?,
                None => reader.next_byte()?,
            };

            if byte == 0xFF {
                let mut next_byte = reader.next_byte()?;

                // Check for byte stuffing.
                if next_byte != 0x00 {
                    // We seem to have reached the end of entropy-coded data and encountered a
                    // marker. Since we can't put data back into the reader, we have to continue
                    // reading to identify the marker so we can pass it on.

                    // Section B.1.1.2
                    // "Any marker may optionally be preceded by any number of fill bytes, which are bytes assigned code X’FF’."
                    while next_byte == 0xFF {
                        next_byte = reader.next_byte()?;
                    }

                    match next_byte {
                        0x00 => {
                            return Err(Error::Format(
                                "FF 00 found where marker was expected".to_owned(),
                            ))
                        }
                        _ => self.marker = Some(Marker::from_u8(next_byte).unwrap()),
                    }

                    continue;
                }
            }

            self.bits |= (byte as u64) << (56 - self.num_bits);
            self.num_bits += 8;
        }

        Ok(())
    }
}

// Section F.2.2.1
// Figure F.12
fn extend(value: u16, count: u8) -> i16 {
    let vt = 1 << (count as u16 - 1);

    if value < vt {
        value as i16 + (-1 << count as i16) + 1
    } else {
        value as i16
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HuffmanTableClass {
    DC,
    AC,
}

pub struct HuffmanTable {
    values: Vec<u8>,
    delta: [i32; 16],
    maxcode: [i32; 16],

    lut: [(u8, u8); 1 << LUT_BITS],
    /// Combined run/value decoding of the SAME bit window as `lut`. Kept as a
    /// separate small array on purpose: folding both into one 8-byte-padded
    /// entry removed a redundant peek but quadrupled the primary table to 2 KB,
    /// and measured 1703 -> 1969 ms. The redundancy was worth less than the
    /// cache footprint. A DC table leaves this all-zero.
    ac_lut: [(i16, u8); 1 << LUT_BITS],
}

impl HuffmanTable {
    pub fn new(bits: &[u8; 16], values: &[u8], class: HuffmanTableClass) -> Result<HuffmanTable> {
        let (huffcode, huffsize) = derive_huffman_codes(bits)?;

        // Section F.2.2.3
        // Figure F.15
        // delta[i] is set to VALPTR(I) - MINCODE(I)
        let mut delta = [0i32; 16];
        let mut maxcode = [-1i32; 16];
        let mut j = 0;

        for i in 0..16 {
            if bits[i] != 0 {
                delta[i] = j as i32 - huffcode[j] as i32;
                j += bits[i] as usize;
                maxcode[i] = huffcode[j - 1] as i32;
            }
        }

        // Build a lookup table for faster decoding.
        let mut lut = [(0u8, 0u8); 1 << LUT_BITS];

        for (i, &size) in huffsize
            .iter()
            .enumerate()
            .filter(|&(_, &size)| size <= LUT_BITS)
        {
            let bits_remaining = LUT_BITS - size;
            let start = (huffcode[i] << bits_remaining) as usize;

            let val = (values[i], size);
            for b in &mut lut[start..][..1 << bits_remaining] {
                *b = val;
            }
        }

        // Build a lookup table for small AC coefficients which both decodes the value and does the
        // equivalent of receive_extend.
        let mut ac_lut = [(0i16, 0u8); 1 << LUT_BITS];
        if matches!(class, HuffmanTableClass::AC) {
            for (i, &(value, size)) in lut.iter().enumerate() {
                let run_length = value >> 4;
                let magnitude_category = value & 0x0f;

                if magnitude_category > 0 && size + magnitude_category <= LUT_BITS {
                    let unextended_ac_value = (((i << size) & ((1 << LUT_BITS) - 1))
                        >> (LUT_BITS - magnitude_category))
                        as u16;
                    ac_lut[i] = (
                        extend(unextended_ac_value, magnitude_category),
                        (run_length << 4) | (size + magnitude_category),
                    );
                }
            }
        }

        Ok(HuffmanTable {
            values: values.to_vec(),
            delta,
            maxcode,
            lut,
            ac_lut,
        })
    }
}

// Section C.2
fn derive_huffman_codes(bits: &[u8; 16]) -> Result<(Vec<u16>, Vec<u8>)> {
    // Figure C.1
    let huffsize = bits
        .iter()
        .enumerate()
        .fold(Vec::new(), |mut acc, (i, &value)| {
            acc.extend(std::iter::repeat_n((i + 1) as u8, value as usize));
            acc
        });

    // Figure C.2
    let mut huffcode = vec![0u16; huffsize.len()];
    let mut code_size = huffsize[0];
    let mut code = 0u32;

    for (i, &size) in huffsize.iter().enumerate() {
        while code_size < size {
            code <<= 1;
            code_size += 1;
        }

        if code >= (1u32 << size) {
            return Err(Error::Format("bad huffman code length".to_owned()));
        }

        huffcode[i] = code as u16;
        code += 1;
    }

    Ok((huffcode, huffsize))
}

// https://www.loc.gov/preservation/digital/formats/fdd/fdd000063.shtml
// "Avery Lee, writing in the rec.video.desktop newsgroup in 2001, commented that "MJPEG, or at
//  least the MJPEG in AVIs having the MJPG fourcc, is restricted JPEG with a fixed -- and
//  *omitted* -- Huffman table. The JPEG must be YCbCr colorspace, it must be 4:2:2, and it must
//  use basic Huffman encoding, not arithmetic or progressive.... You can indeed extract the
//  MJPEG frames and decode them with a regular JPEG decoder, but you have to prepend the DHT
//  segment to them, or else the decoder won't have any idea how to decompress the data.
//  The exact table necessary is given in the OpenDML spec.""
pub fn fill_default_mjpeg_tables(
    scan: &ScanInfo,
    dc_huffman_tables: &mut [Option<HuffmanTable>],
    ac_huffman_tables: &mut [Option<HuffmanTable>],
) {
    // Section K.3.3

    if dc_huffman_tables[0].is_none() && scan.dc_table_indices.contains(&0) {
        // Table K.3
        dc_huffman_tables[0] = Some(
            HuffmanTable::new(
                &[
                    0x00, 0x01, 0x05, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00,
                ],
                &[
                    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B,
                ],
                HuffmanTableClass::DC,
            )
            .unwrap(),
        );
    }
    if dc_huffman_tables[1].is_none() && scan.dc_table_indices.contains(&1) {
        // Table K.4
        dc_huffman_tables[1] = Some(
            HuffmanTable::new(
                &[
                    0x00, 0x03, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00,
                    0x00, 0x00, 0x00,
                ],
                &[
                    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B,
                ],
                HuffmanTableClass::DC,
            )
            .unwrap(),
        );
    }
    if ac_huffman_tables[0].is_none() && scan.ac_table_indices.contains(&0) {
        // Table K.5
        ac_huffman_tables[0] = Some(
            HuffmanTable::new(
                &[
                    0x00, 0x02, 0x01, 0x03, 0x03, 0x02, 0x04, 0x03, 0x05, 0x05, 0x04, 0x04, 0x00,
                    0x00, 0x01, 0x7D,
                ],
                &[
                    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13,
                    0x51, 0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xA1, 0x08, 0x23, 0x42,
                    0xB1, 0xC1, 0x15, 0x52, 0xD1, 0xF0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0A,
                    0x16, 0x17, 0x18, 0x19, 0x1A, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x34, 0x35,
                    0x36, 0x37, 0x38, 0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A,
                    0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67,
                    0x68, 0x69, 0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x83, 0x84,
                    0x85, 0x86, 0x87, 0x88, 0x89, 0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98,
                    0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xB2, 0xB3,
                    0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7,
                    0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8, 0xD9, 0xDA, 0xE1,
                    0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xF1, 0xF2, 0xF3, 0xF4,
                    0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA,
                ],
                HuffmanTableClass::AC,
            )
            .unwrap(),
        );
    }
    if ac_huffman_tables[1].is_none() && scan.ac_table_indices.contains(&1) {
        // Table K.6
        ac_huffman_tables[1] = Some(
            HuffmanTable::new(
                &[
                    0x00, 0x02, 0x01, 0x02, 0x04, 0x04, 0x03, 0x04, 0x07, 0x05, 0x04, 0x04, 0x00,
                    0x01, 0x02, 0x77,
                ],
                &[
                    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51,
                    0x07, 0x61, 0x71, 0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xA1, 0xB1,
                    0xC1, 0x09, 0x23, 0x33, 0x52, 0xF0, 0x15, 0x62, 0x72, 0xD1, 0x0A, 0x16, 0x24,
                    0x34, 0xE1, 0x25, 0xF1, 0x17, 0x18, 0x19, 0x1A, 0x26, 0x27, 0x28, 0x29, 0x2A,
                    0x35, 0x36, 0x37, 0x38, 0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
                    0x4A, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66,
                    0x67, 0x68, 0x69, 0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x82,
                    0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8A, 0x92, 0x93, 0x94, 0x95, 0x96,
                    0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA,
                    0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3, 0xC4, 0xC5,
                    0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8, 0xD9,
                    0xDA, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xF2, 0xF3, 0xF4,
                    0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA,
                ],
                HuffmanTableClass::AC,
            )
            .unwrap(),
        );
    }
}
