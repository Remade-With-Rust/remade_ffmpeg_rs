//! Output channel configuration: which channel element feeds which output
//! channel, in which order.
//!
//! AAC carries channels as *elements* (SCE/CPE/LFE, plus CCE coupling) named by a
//! 4-bit tag. The program — a `channelConfiguration` index or a PCE — says where
//! each element sits (front/side/back/LFE). This module turns that into an
//! ordered output list using the **same model FFmpeg's decoder uses**: the
//! elements are assigned speaker positions per height layer, sorted by the
//! position's bit in the WAVE/FFmpeg channel mask, and routed to by tag (PCE
//! streams) or by arrival order with the documented fallbacks for mis-tagged
//! streams (indexed configurations). Matching that model exactly is what makes
//! multichannel output sample-for-sample comparable with FFmpeg.

use crate::config::Pce;

pub const TYPE_SCE: u8 = 0;
pub const TYPE_CPE: u8 = 1;
pub const TYPE_CCE: u8 = 2;
pub const TYPE_LFE: u8 = 3;

/// `enum ChannelPosition` (speaker group of an element).
pub const POS_OFF: u8 = 0;
pub const POS_FRONT: u8 = 1;
pub const POS_SIDE: u8 = 2;
pub const POS_BACK: u8 = 3;
pub const POS_LFE: u8 = 4;
pub const POS_CC: u8 = 5;

/// One `(syn_ele, elem_id, position)` row of a layout map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tag {
    pub syn_ele: u8,
    pub id: u8,
    pub pos: u8,
}

const fn t(syn_ele: u8, id: u8, pos: u8) -> Tag {
    Tag { syn_ele, id, pos }
}

/// Element rows per `channelConfiguration` (ISO Table 1.19 / 23003-3).
pub fn default_layout(cc: u8, strict_71_wide: bool) -> Option<Vec<Tag>> {
    let (s, c, l) = (TYPE_SCE, TYPE_CPE, TYPE_LFE);
    let (f, b, si, lf) = (POS_FRONT, POS_BACK, POS_SIDE, POS_LFE);
    let mut v = match cc {
        1 => vec![t(s, 0, f)],
        2 => vec![t(c, 0, f)],
        3 => vec![t(s, 0, f), t(c, 0, f)],
        4 => vec![t(s, 0, f), t(c, 0, f), t(s, 1, b)],
        5 => vec![t(s, 0, f), t(c, 0, f), t(c, 1, b)],
        6 => vec![t(s, 0, f), t(c, 0, f), t(c, 1, b), t(l, 0, lf)],
        7 => vec![t(s, 0, f), t(c, 0, f), t(c, 1, f), t(c, 2, b), t(l, 0, lf)],
        11 => vec![t(s, 0, f), t(c, 0, f), t(c, 1, b), t(s, 1, b), t(l, 0, lf)],
        12 => vec![t(s, 0, f), t(c, 0, f), t(c, 1, b), t(c, 2, b), t(l, 0, lf)],
        13 => vec![
            t(s, 0, f),
            t(c, 0, f),
            t(c, 1, f),
            t(c, 2, b),
            t(c, 3, b),
            t(s, 1, b),
            t(l, 0, lf),
            t(l, 1, lf),
            t(s, 2, f),
            t(c, 4, f),
            t(c, 5, si),
            t(s, 3, si),
            t(c, 6, b),
            t(s, 4, b),
            t(s, 5, f),
            t(c, 7, f),
        ],
        14 => vec![t(s, 0, f), t(c, 0, f), t(c, 1, b), t(l, 0, lf), t(c, 2, f)],
        _ => return None,
    };
    // ISO says configuration 7 is 7.1(wide) (a second FRONT pair); in practice
    // encoders use it for 7.1 with side/back surrounds, and FFmpeg decodes it
    // that way unless asked to be strict. Follow it, so outputs line up.
    if cc == 7 && !strict_71_wide {
        v[2].pos = b;
    }
    Some(v)
}

/// The rows a PCE describes, in bitstream order (front, side, back, LFE, CC),
/// rearranged by height layer when the comment carries the 0xAC height
/// extension.
pub fn pce_layout(p: &Pce) -> Vec<Tag> {
    let mut rows = Vec::new();
    for e in &p.front {
        rows.push(t(
            if e.is_cpe { TYPE_CPE } else { TYPE_SCE },
            e.tag,
            POS_FRONT,
        ));
    }
    for e in &p.side {
        rows.push(t(
            if e.is_cpe { TYPE_CPE } else { TYPE_SCE },
            e.tag,
            POS_SIDE,
        ));
    }
    for e in &p.back {
        rows.push(t(
            if e.is_cpe { TYPE_CPE } else { TYPE_SCE },
            e.tag,
            POS_BACK,
        ));
    }
    for &tag in &p.lfe {
        rows.push(t(TYPE_LFE, tag, POS_LFE));
    }
    for &(_, tag) in &p.cc {
        rows.push(t(TYPE_CCE, tag, POS_CC));
    }
    let (nf, ns, nb) = (p.front.len(), p.side.len(), p.back.len());
    if let Some(heights) = height_extension(&p.comment, nf + ns + nb) {
        let positional = &rows[..nf + ns + nb];
        let rest = rows[nf + ns + nb..].to_vec();
        let mut out = Vec::new();
        for layer in 0..3u8 {
            for (i, r) in positional.iter().enumerate() {
                if heights[i] == layer {
                    out.push(*r);
                }
            }
            if layer == 0 {
                out.extend_from_slice(&rest);
            }
        }
        return out;
    }
    rows
}

/// The PCE height extension carried in the comment field (ISO 14496-3, PCE
/// `height_extension_element`): sync byte 0xAC, 2 bits of height per
/// front/side/back element (0 normal, 1 top, 2 bottom), byte alignment, then a
/// CRC-8 (x^8+x^2+x+1, init 0xFF) over the sync byte and the height bytes. The
/// heights are honoured only when the CRC verifies — a comment that merely
/// starts with 0xAC must not move speakers.
fn height_extension(c: &[u8], n: usize) -> Option<Vec<u8>> {
    let hbytes = (2 * n).div_ceil(8);
    if n == 0 || c.len() < 2 + hbytes || c[0] != 0xAC {
        return None;
    }
    let mut crc = 0xFFu8;
    for &b in &c[..1 + hbytes] {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
        }
    }
    if crc != c[1 + hbytes] {
        return None;
    }
    let mut heights = Vec::with_capacity(n);
    for i in 0..n {
        let byte = c[1 + i / 4];
        let h = (byte >> (6 - 2 * (i % 4))) & 3;
        if h > 2 {
            return None;
        }
        heights.push(h);
    }
    Some(heights)
}

// AVChannel numbers (bit index in the channel mask).
const FL: i16 = 0;
const FR: i16 = 1;
const FC: i16 = 2;
const LFE: i16 = 3;
const BL: i16 = 4;
const BR: i16 = 5;
const FLC: i16 = 6;
const FRC: i16 = 7;
const BC: i16 = 8;
const SL: i16 = 9;
const SR: i16 = 10;
const TC: i16 = 11;
const TFL: i16 = 12;
const TFC: i16 = 13;
const TFR: i16 = 14;
const TBL: i16 = 15;
const TBC: i16 = 16;
const TBR: i16 = 17;
const LFE2: i16 = 35;
const TSL: i16 = 36;
const TSR: i16 = 37;
const BFC: i16 = 38;
const BFL: i16 = 39;
const BFR: i16 = 40;
const NONE: i16 = -1;
const UNUSED: i16 = 0x400;

/// Speaker assignment per height layer and position group.
const CHANNEL_MAP: [[[i16; 6]; 4]; 3] = [
    [
        [FC, FLC, FRC, FL, FR, NONE],
        [UNUSED, SL, SR, NONE, NONE, NONE],
        [UNUSED, SL, SR, BL, BR, BC],
        [LFE, LFE2, NONE, NONE, NONE, NONE],
    ],
    [
        [TFC, NONE, NONE, TFL, TFR, NONE],
        [UNUSED, TSL, TSR, NONE, NONE, TC],
        [UNUSED, NONE, NONE, TBL, TBR, TBC],
        [NONE, NONE, NONE, NONE, NONE, NONE],
    ],
    [
        [BFC, NONE, NONE, BFL, BFR, NONE],
        [NONE, NONE, NONE, NONE, NONE, NONE],
        [NONE, NONE, NONE, NONE, NONE, NONE],
        [NONE, NONE, NONE, NONE, NONE, NONE],
    ],
];

const LAYOUT_22POINT2: u64 = (1 << FL)
    | (1 << FR)
    | (1 << FC)
    | (1 << LFE)
    | (1 << BL)
    | (1 << BR)
    | (1 << FLC)
    | (1 << FRC)
    | (1 << BC)
    | (1 << SL)
    | (1 << SR)
    | (1 << TC)
    | (1 << TFL)
    | (1 << TFC)
    | (1 << TFR)
    | (1 << TBL)
    | (1 << TBC)
    | (1 << TBR)
    | (1u64 << LFE2)
    | (1u64 << TSL)
    | (1u64 << TSR)
    | (1u64 << BFC)
    | (1u64 << BFL)
    | (1u64 << BFR);

#[derive(Clone, Copy)]
struct E2c {
    av_position: u64,
    syn_ele: u8,
    elem_id: u8,
    aac_position: u8,
}

const E2C_ZERO: E2c = E2c {
    av_position: 0,
    syn_ele: 0,
    elem_id: 0,
    aac_position: 0,
};

fn bit(ch: i16) -> u64 {
    if ch < 0 {
        u64::MAX
    } else {
        1u64 << ch
    }
}

fn count_paired_channels(map: &[Tag], pos: u8, current: usize) -> i32 {
    let (mut n, mut first_cpe, mut sce_parity) = (0i32, false, false);
    for row in &map[current..] {
        if row.pos != pos {
            break;
        }
        if row.syn_ele == TYPE_CPE {
            if sce_parity {
                if pos == POS_FRONT && !first_cpe {
                    sce_parity = false;
                } else {
                    return -1;
                }
            }
            n += 2;
            first_cpe = true;
        } else {
            n += 1;
            sce_parity ^= pos != POS_LFE;
        }
    }
    if sce_parity && pos == POS_FRONT && first_cpe {
        return -1;
    }
    n
}

fn assign_pair(
    e2c: &mut [E2c],
    map: &[Tag],
    off: usize,
    left: u64,
    right: u64,
    pos: u8,
    layout: &mut u64,
) -> usize {
    if map[off].syn_ele == TYPE_CPE {
        e2c[off] = E2c {
            av_position: left | right,
            syn_ele: TYPE_CPE,
            elem_id: map[off].id,
            aac_position: pos,
        };
        if e2c[off].av_position != u64::MAX {
            *layout |= e2c[off].av_position;
        }
        1
    } else {
        e2c[off] = E2c {
            av_position: left,
            syn_ele: TYPE_SCE,
            elem_id: map[off].id,
            aac_position: pos,
        };
        e2c[off + 1] = E2c {
            av_position: right,
            syn_ele: TYPE_SCE,
            elem_id: map[off + 1].id,
            aac_position: pos,
        };
        if left != u64::MAX {
            *layout |= left;
        }
        if right != u64::MAX {
            *layout |= right;
        }
        2
    }
}

/// Returns Err(()) for FFmpeg's "-1" (layout cannot be sniffed).
fn assign_channels(
    e2c: &mut [E2c],
    map: &[Tag],
    layout: &mut u64,
    layer: usize,
    pos: u8,
    current: &mut usize,
) -> Result<(), ()> {
    let mut i = *current;
    let mut nb = count_paired_channels(map, pos, i);
    if !(0..=5).contains(&nb) {
        return Ok(());
    }
    let cm = &CHANNEL_MAP[layer][(pos - 1) as usize];
    if pos == POS_LFE {
        let mut j = 0;
        while nb > 0 {
            if cm[j] == NONE {
                return Err(());
            }
            e2c[i] = E2c {
                av_position: bit(cm[j]),
                syn_ele: map[i].syn_ele,
                elem_id: map[i].id,
                aac_position: pos,
            };
            *layout |= e2c[i].av_position;
            i += 1;
            j += 1;
            nb -= 1;
        }
        *current = i;
        return Ok(());
    }
    while nb & 1 == 1 {
        if cm[0] == NONE {
            return Err(());
        }
        if cm[0] == UNUSED {
            break;
        }
        e2c[i] = E2c {
            av_position: bit(cm[0]),
            syn_ele: map[i].syn_ele,
            elem_id: map[i].id,
            aac_position: pos,
        };
        *layout |= e2c[i].av_position;
        i += 1;
        nb -= 1;
    }
    let mut j = if pos != POS_SIDE && nb <= 3 { 3 } else { 1 };
    while nb >= 2 {
        if cm[j] == NONE || cm[j + 1] == NONE {
            return Err(());
        }
        i += assign_pair(e2c, map, i, bit(cm[j]), bit(cm[j + 1]), pos, layout);
        j += 2;
        nb -= 2;
    }
    while nb & 1 == 1 {
        if cm[5] == NONE {
            return Err(());
        }
        e2c[i] = E2c {
            av_position: bit(cm[5]),
            syn_ele: map[i].syn_ele,
            elem_id: map[i].id,
            aac_position: pos,
        };
        *layout |= e2c[i].av_position;
        i += 1;
        nb -= 1;
    }
    if nb != 0 {
        return Err(());
    }
    *current = i;
    Ok(())
}

/// FFmpeg's `sniff_channel_order`: reorders `map` (non-CC rows) into output
/// order and returns the channel mask (0 = unknown order, keep PCE order).
fn sniff_channel_order(map: &mut [Tag]) -> u64 {
    let tags = map.len();
    let mut e2c = vec![E2C_ZERO; tags.max(1) * 2 + 8];
    let mut layout = 0u64;
    let mut i = 0usize;
    let mut n = 0usize;
    while n < 3 && i < tags {
        for pos in [POS_FRONT, POS_SIDE, POS_BACK, POS_LFE] {
            if assign_channels(&mut e2c, map, &mut layout, n, pos, &mut i).is_err() {
                return 0;
            }
        }
        n += 1;
    }
    let total = i;
    let mut n = i;
    if layout == LAYOUT_22POINT2 {
        e2c.swap(2, 0);
        e2c.swap(2, 1);
        e2c.swap(6, 2);
        e2c.swap(4, 3);
        e2c.swap(6, 4);
        e2c.swap(7, 6);
        e2c.swap(9, 8);
        e2c.swap(11, 10);
        e2c.swap(12, 11);
    } else {
        loop {
            let mut next_n = 0;
            for k in 1..n {
                if e2c[k - 1].av_position > e2c[k].av_position {
                    e2c.swap(k - 1, k);
                    next_n = k;
                }
            }
            n = next_n;
            if n == 0 {
                break;
            }
        }
    }
    for k in 0..total {
        map[k] = Tag {
            syn_ele: e2c[k].syn_ele,
            id: e2c[k].elem_id,
            pos: e2c[k].aac_position,
        };
    }
    layout
}

/// The resolved output configuration.
#[derive(Debug, Clone, Default)]
pub struct OutputConfig {
    /// The layout rows after sniffing (output order for the positional rows).
    pub map: Vec<Tag>,
    /// Output channels: (element type, element INSTANCE index, sub-channel).
    pub outputs: Vec<(u8, u8, u8)>,
    /// `tag_che_map`: (type, tag) -> instance index.
    pub tag_to_instance: [[Option<u8>; 16]; 4],
    /// Instances allocated per type (`che[type][iid]`).
    pub allocated: [[bool; 16]; 4],
    /// Channel mask of the sniffed layout (0 when the order is the PCE's own).
    pub mask: u64,
}

impl OutputConfig {
    /// `ff_aac_output_configure`: allocate elements and fix the output order.
    /// `ps` = an SCE carries Parametric Stereo (two outputs per mono element).
    pub fn configure(mut map: Vec<Tag>, ps: bool) -> OutputConfig {
        let mut oc = OutputConfig::default();
        let mut id_map = [[0u8; 16]; 4];
        let mut counts = [0u8; 4];
        for r in &map {
            let ty = (r.syn_ele & 3) as usize;
            id_map[ty][(r.id & 15) as usize] = counts[ty];
            counts[ty] = counts[ty].saturating_add(1);
        }
        oc.mask = sniff_channel_order(&mut map);
        for r in &map {
            let ty = (r.syn_ele & 3) as usize;
            let iid = id_map[ty][(r.id & 15) as usize] & 15;
            if r.pos != POS_OFF {
                oc.allocated[ty][iid as usize] = true;
                if r.syn_ele != TYPE_CCE {
                    oc.outputs.push((r.syn_ele, iid, 0));
                    if r.syn_ele == TYPE_CPE || (r.syn_ele == TYPE_SCE && ps) {
                        oc.outputs.push((r.syn_ele, iid, 1));
                    }
                }
            }
            oc.tag_to_instance[ty][(r.id & 15) as usize] = Some(iid);
        }
        oc.map = map;
        oc
    }

    pub fn channels(&self) -> usize {
        self.outputs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(cc: u8) -> Vec<(u8, u8, u8)> {
        OutputConfig::configure(default_layout(cc, false).unwrap(), false).outputs
    }

    /// 5.1 (config 6): FFmpeg emits FL FR FC LFE BL BR — the CPE, then the
    /// centre SCE, the LFE, the back CPE.
    #[test]
    fn config6_is_fl_fr_fc_lfe_bl_br() {
        assert_eq!(
            order(6),
            vec![
                (TYPE_CPE, 0, 0),
                (TYPE_CPE, 0, 1),
                (TYPE_SCE, 0, 0),
                (TYPE_LFE, 0, 0),
                (TYPE_CPE, 1, 0),
                (TYPE_CPE, 1, 1)
            ]
        );
    }

    #[test]
    fn config3_is_fl_fr_fc() {
        assert_eq!(
            order(3),
            vec![(TYPE_CPE, 0, 0), (TYPE_CPE, 0, 1), (TYPE_SCE, 0, 0)]
        );
    }

    /// FATE `al22_chCfg0PCE_44`: the PCE comment `ac 04 2f` is a valid height
    /// extension (CRC 0x2f) putting the third front pair in the TOP layer.
    #[test]
    fn pce_height_extension_with_valid_crc() {
        assert_eq!(
            height_extension(&[0xAC, 0x04, 0x2F], 4),
            Some(vec![0, 0, 1, 0])
        );
        assert_eq!(
            height_extension(&[0xAC, 0x04, 0x2E], 4),
            None,
            "bad CRC ignored"
        );
        assert_eq!(height_extension(b"Encoded by", 4), None);
    }

    #[test]
    fn mono_and_stereo_are_identity() {
        assert_eq!(order(1), vec![(TYPE_SCE, 0, 0)]);
        assert_eq!(order(2), vec![(TYPE_CPE, 0, 0), (TYPE_CPE, 0, 1)]);
    }
}
