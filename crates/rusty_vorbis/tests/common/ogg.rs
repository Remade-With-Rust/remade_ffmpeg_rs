//! Minimal Ogg page reader for tests, examples and benches (NOT part of the
//! library): splits the first logical bitstream into packets and records the
//! granule position of the page each packet completes on.
//!
//! Included with `#[path = ".../tests/common/ogg.rs"] mod ogg;` so the
//! decoder crate stays Ogg-agnostic.

#![allow(dead_code)]

/// One packet of the first logical stream.
pub struct OggPacket {
    pub data: Vec<u8>,
    /// Granule position of the page this packet completed on, if it was the
    /// last packet completed on that page (else -1).
    pub granule: i64,
}

/// Split an Ogg file into the packets of its first logical stream.
pub fn read_packets(file: &[u8]) -> Vec<OggPacket> {
    let mut out = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut serial: Option<u32> = None;
    let mut p = 0usize;
    while p + 27 <= file.len() {
        if &file[p..p + 4] != b"OggS" {
            p += 1;
            continue;
        }
        let granule = i64::from_le_bytes(file[p + 6..p + 14].try_into().unwrap());
        let ser = u32::from_le_bytes(file[p + 14..p + 18].try_into().unwrap());
        let nseg = file[p + 26] as usize;
        let lacing = &file[p + 27..(p + 27 + nseg).min(file.len())];
        let mut d = p + 27 + nseg;
        let mine = *serial.get_or_insert(ser) == ser;
        let mut completed = Vec::new();
        for &l in lacing {
            let end = (d + l as usize).min(file.len());
            if mine {
                cur.extend_from_slice(&file[d..end]);
                if l < 255 {
                    completed.push(std::mem::take(&mut cur));
                }
            }
            d += l as usize;
        }
        let n = completed.len();
        for (i, data) in completed.into_iter().enumerate() {
            out.push(OggPacket {
                data,
                granule: if i + 1 == n { granule } else { -1 },
            });
        }
        p = d;
    }
    out
}

fn ogg_crc(data: &[u8]) -> u32 {
    // Ogg CRC-32: poly 0x04c11db7, init 0, unreflected, no final xor.
    let mut crc: u32 = 0;
    for &b in data {
        crc ^= u32::from(b) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn ogg_page(serial: u32, seq: u32, granule: i64, htype: u8, packet: &[u8]) -> Vec<u8> {
    let mut segs = Vec::new();
    let mut l = packet.len();
    while l >= 255 {
        segs.push(255u8);
        l -= 255;
    }
    segs.push(l as u8);
    assert!(
        segs.len() <= 255,
        "test pager: packet too large for one page"
    );
    let mut page = Vec::new();
    page.extend_from_slice(b"OggS");
    page.push(0);
    page.push(htype);
    page.extend_from_slice(&granule.to_le_bytes());
    page.extend_from_slice(&serial.to_le_bytes());
    page.extend_from_slice(&seq.to_le_bytes());
    page.extend_from_slice(&[0, 0, 0, 0]);
    page.push(segs.len() as u8);
    page.extend_from_slice(&segs);
    page.extend_from_slice(packet);
    let crc = ogg_crc(&page);
    page[22..26].copy_from_slice(&crc.to_le_bytes());
    page
}

/// Write a single-stream Ogg file, one packet per page; `granules[i]` is the
/// granule position of audio packet `i`.
pub fn write_ogg(headers: &[Vec<u8>], audio: &[Vec<u8>], granules: &[i64]) -> Vec<u8> {
    let serial = 0x5EED_0001;
    let mut out = Vec::new();
    for (i, h) in headers.iter().enumerate() {
        out.extend(ogg_page(
            serial,
            i as u32,
            0,
            if i == 0 { 0x02 } else { 0 },
            h,
        ));
    }
    for (i, (p, &g)) in audio.iter().zip(granules).enumerate() {
        let htype = if i + 1 == audio.len() { 0x04 } else { 0 };
        out.extend(ogg_page(serial, (headers.len() + i) as u32, g, htype, p));
    }
    out
}
