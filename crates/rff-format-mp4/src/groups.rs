//! Sample groups: `sgpd` (the group descriptions) and `sbgp` (which samples
//! are in which group), ISO/IEC 14496-12 8.9, from a track's `stbl` and from
//! the `traf`s of a fragmented file.
//!
//! Compact sample-to-group boxes (`csgp`) are not read.

use rff_format::{SampleGroupRun, SampleGrouping};

use super::be32;

/// The most groupings (distinct type and parameter) read per track. Real
/// files have a handful; it bounds the searches a hostile file can cause.
const MAX_GROUPINGS: usize = 32;
/// The most descriptions read from one `sgpd`: an `sbgp` index past it means
/// a fragment's own description.
const MAX_DESCRIPTIONS: usize = 1 << 16;

struct Sgpd {
    typ: [u8; 4],
    descriptions: Vec<Vec<u8>>,
    default: Option<usize>,
}

struct Sbgp {
    typ: [u8; 4],
    parameter: Option<u32>,
    /// `(sample_count, group_description_index)`.
    runs: Vec<(u32, u32)>,
}

fn fourcc(b: &[u8], at: usize) -> Option<[u8; 4]> {
    b.get(at..at + 4)?.try_into().ok()
}

/// The entries of an `sgpd` whose entry count is at `at`: each of
/// `default_length` bytes, or behind its own length when that is 0. `None`
/// when the box does not hold them all.
fn entries(body: &[u8], at: usize, default_length: u32) -> Option<Vec<Vec<u8>>> {
    let count = u32::from_be_bytes(fourcc(body, at)?) as usize;
    let mut rest = body.get(at + 4..)?;
    if count > MAX_DESCRIPTIONS {
        return None;
    }
    let mut out = Vec::new();
    for _ in 0..count {
        let len = if default_length != 0 {
            default_length as usize
        } else {
            let len = u32::from_be_bytes(fourcc(rest, 0)?) as usize;
            rest = &rest[4..];
            len
        };
        out.push(rest.get(..len)?.to_vec());
        rest = &rest[len..];
    }
    Some(out)
}

fn parse_sgpd(body: &[u8]) -> Option<Sgpd> {
    let version = *body.first()?;
    let typ = fourcc(body, 4)?;
    let (descriptions, default) = match version {
        0 => {
            // No lengths (deprecated): all entries are the size the type
            // says, or share what is there equally.
            let count = be32(body, 8) as usize;
            let rest = body.get(12..)?;
            if count == 0 || count > MAX_DESCRIPTIONS || rest.len() % count != 0 {
                return None;
            }
            match rest.len() / count {
                0 => return None,
                n => (rest.chunks_exact(n).map(<[u8]>::to_vec).collect(), 0),
            }
        }
        1 => (entries(body, 12, be32(body, 8))?, 0),
        // `default_length` (absent in the first edition that had version 2,
        // which is tried when this layout does not fit) then the default
        // group.
        _ => match entries(body, 16, be32(body, 8)) {
            Some(d) => (d, be32(body, 12)),
            None => {
                let count = be32(body, 12) as usize;
                let rest = body.get(16..)?;
                if count == 0 || count > MAX_DESCRIPTIONS || rest.len() % count != 0 {
                    return None;
                }
                match rest.len() / count {
                    0 => return None,
                    n => (
                        rest.chunks_exact(n).map(<[u8]>::to_vec).collect(),
                        be32(body, 8),
                    ),
                }
            }
        },
    };
    let default = (default as usize)
        .checked_sub(1)
        .filter(|&i| i < descriptions.len());
    Some(Sgpd {
        typ,
        descriptions,
        default,
    })
}

fn parse_sbgp(body: &[u8]) -> Option<Sbgp> {
    let version = *body.first()?;
    let typ = fourcc(body, 4)?;
    let (parameter, at) = if version >= 1 {
        (Some(u32::from_be_bytes(fourcc(body, 8)?)), 12)
    } else {
        (None, 8)
    };
    let count = u32::from_be_bytes(fourcc(body, at)?) as usize;
    let runs = body
        .get(at + 4..)?
        .chunks_exact(8)
        .take(count)
        .map(|r| (be32(r, 0), be32(r, 4)))
        .collect();
    Some(Sbgp {
        typ,
        parameter,
        runs,
    })
}

fn boxes(kids: &[([u8; 4], &[u8])]) -> (Vec<Sgpd>, Vec<Sbgp>) {
    let of = |t: &'static [u8; 4]| {
        kids.iter()
            .filter(move |(k, _)| k == t)
            .take(MAX_GROUPINGS)
            .map(|(_, b)| *b)
    };
    (
        of(b"sgpd").filter_map(parse_sgpd).collect(),
        of(b"sbgp").filter_map(parse_sbgp).collect(),
    )
}

/// A track's groupings while its boxes are read.
#[derive(Default)]
pub(crate) struct GroupTable {
    /// Each with how many of its descriptions are the movie's (a fragment's
    /// own come after them).
    list: Vec<(SampleGrouping, usize)>,
    /// `sgpd`s of the movie, which a fragment's `sbgp` may refer to.
    movie: Vec<Sgpd>,
    /// Samples the group numbering counts beyond the stored table's entries
    /// (uncompressed audio indexed by chunk).
    bias: u64,
}

/// Append `count` samples to `g`, joined to the last run when they follow it
/// and are in the same group.
fn push_run(g: &mut SampleGrouping, first: u64, count: u32, description: Option<usize>) {
    if count == 0 {
        return;
    }
    if let Some(last) = g.runs.last_mut() {
        if last.first_sample + u64::from(last.sample_count) == first
            && last.description == description
        {
            if let Some(n) = last.sample_count.checked_add(count) {
                last.sample_count = n;
                return;
            }
        }
    }
    g.runs.push(SampleGroupRun::new(first, count, description));
}

impl GroupTable {
    /// The groupings of a track's sample table. `bias` is how many more
    /// samples the groups count than the stored table has entries.
    pub(crate) fn from_stbl(stbl: &[([u8; 4], &[u8])], bias: u64) -> GroupTable {
        let (movie, sbgps) = boxes(stbl);
        let mut t = GroupTable {
            list: Vec::new(),
            movie,
            bias,
        };
        for sbgp in sbgps {
            let Some(at) = t.grouping(sbgp.typ, sbgp.parameter) else {
                break;
            };
            let (g, movie_n) = &mut t.list[at];
            if !g.runs.is_empty() {
                continue; // a second box of the same grouping
            }
            let mut first = 0u64;
            for (count, index) in sbgp.runs {
                let d = (index as usize).checked_sub(1).filter(|i| i < movie_n);
                push_run(g, first, count, d);
                first += u64::from(count);
            }
        }
        // Descriptions no `sbgp` assigns: a default group covers every
        // sample, and fragments may assign the rest.
        for i in 0..t.movie.len() {
            let typ = t.movie[i].typ;
            if !t.list.iter().any(|(g, _)| g.grouping_type == typ) {
                t.grouping(typ, None);
            }
        }
        t
    }

    /// The position of the grouping `(typ, parameter)`, made with the movie's
    /// descriptions of that type when it is new; `None` past the limit.
    fn grouping(&mut self, typ: [u8; 4], parameter: Option<u32>) -> Option<usize> {
        let found = self
            .list
            .iter()
            .position(|(g, _)| g.grouping_type == typ && g.parameter == parameter);
        if found.is_some() {
            return found;
        }
        if self.list.len() >= MAX_GROUPINGS {
            return None;
        }
        let mut g = SampleGrouping::new(typ, parameter);
        if let Some(d) = self.movie.iter().find(|d| d.typ == typ) {
            g.descriptions = d.descriptions.clone();
            g.default = d.default;
        }
        let n = g.descriptions.len();
        self.list.push((g, n));
        Some(self.list.len() - 1)
    }

    /// The groupings of one fragment (`traf` children) whose `count` samples
    /// start at stored sample `first`.
    pub(crate) fn add_fragment(&mut self, traf: &[([u8; 4], &[u8])], first: u64, count: u64) {
        if !traf.iter().any(|(t, _)| t == b"sbgp" || t == b"sgpd") {
            return;
        }
        let (local, sbgps) = boxes(traf);
        let first = first.saturating_add(self.bias);
        // Local descriptions with a default group and no `sbgp`.
        let mut work = sbgps;
        for d in &local {
            if !work.iter().any(|s| s.typ == d.typ) {
                work.push(Sbgp {
                    typ: d.typ,
                    parameter: None,
                    runs: Vec::new(),
                });
            }
        }
        for sbgp in work {
            let (typ, parameter, runs) = (sbgp.typ, sbgp.parameter, sbgp.runs);
            let Some(at) = self.grouping(typ, parameter) else {
                break;
            };
            let (g, movie_n) = &mut self.list[at];
            let own = local.iter().find(|d| d.typ == typ);
            let base = g.descriptions.len();
            let own_n = own.map_or(0, |d| d.descriptions.len());
            if let Some(d) = own {
                g.descriptions.extend(d.descriptions.iter().cloned());
            }
            let resolve = |index: u32| -> Option<usize> {
                let i = index as usize;
                match i.checked_sub(0x1_0001) {
                    Some(k) => (k < own_n).then_some(base + k),
                    // Below 0x10001 it counts the movie's descriptions; some
                    // writers count the fragment's when the movie has none.
                    None if *movie_n == 0 => {
                        i.checked_sub(1).filter(|&k| k < own_n).map(|k| base + k)
                    }
                    None => i.checked_sub(1).filter(|k| k < movie_n),
                }
            };
            let mut done = 0u64;
            for (n, index) in runs {
                let n = u64::from(n).min(count - done) as u32;
                push_run(g, first + done, n, resolve(index));
                done += u64::from(n);
            }
            if let Some(default) = own.and_then(|d| d.default) {
                let rest = u32::try_from(count - done).unwrap_or(u32::MAX);
                push_run(g, first + done, rest, Some(base + default));
            }
        }
    }

    /// The finished groupings; those that put no sample anywhere are left out.
    pub(crate) fn finish(self) -> Vec<SampleGrouping> {
        self.list
            .into_iter()
            .map(|(g, _)| g)
            .filter(|g| !g.runs.is_empty() || g.default.is_some() || !g.descriptions.is_empty())
            .collect()
    }
}

/// The frames a chunk-indexed PCM track's groups count beyond its chunks.
pub(crate) fn pcm_bias(stbl: &[([u8; 4], &[u8])], chunk_indexed: bool, stored: usize) -> u64 {
    match super::find(stbl, b"stsz") {
        Some(stsz) if chunk_indexed && be32(stsz, 4) != 0 => {
            u64::from(be32(stsz, 8)).saturating_sub(stored as u64)
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sgpd_versions() {
        // v0: equal shares.
        let v0 = [&[0, 0, 0, 0][..], b"rap ", &[0, 0, 0, 2], &[7, 8]].concat();
        assert_eq!(parse_sgpd(&v0).unwrap().descriptions, [[7], [8]]);
        // v1, own lengths.
        let v1 = [
            &[1, 0, 0, 0][..],
            b"seig",
            &[0, 0, 0, 0],
            &[0, 0, 0, 2],
            &[0, 0, 0, 1, 9],
            &[0, 0, 0, 2, 1, 2],
        ]
        .concat();
        assert_eq!(parse_sgpd(&v1).unwrap().descriptions, [vec![9], vec![1, 2]]);
        // v2, length and default group.
        let v2 = [
            &[2, 0, 0, 0][..],
            b"roll",
            &[0, 0, 0, 2],
            &[0, 0, 0, 1],
            &[0, 0, 0, 1],
            &[0xFF, 0xFF],
        ]
        .concat();
        let d = parse_sgpd(&v2).unwrap();
        assert_eq!((d.descriptions, d.default), (vec![vec![255, 255]], Some(0)));
        // v2 of the edition without lengths.
        let old = [
            &[2, 0, 0, 0][..],
            b"roll",
            &[0, 0, 0, 2],
            &[0, 0, 0, 2],
            &[1, 2, 3, 4],
        ]
        .concat();
        let d = parse_sgpd(&old).unwrap();
        assert_eq!(d.descriptions, [[1, 2], [3, 4]]);
        assert_eq!(d.default, Some(1));
    }

    #[test]
    fn hostile_counts_are_refused_without_allocating() {
        for version in 0..=3u8 {
            for tail in [0usize, 1, 4, 8, 40] {
                let mut b = vec![version, 0, 0, 0];
                b.extend_from_slice(b"roll");
                b.extend_from_slice(&[0xFF; 12]);
                b.resize(b.len() + tail, 0xFF);
                assert!(parse_sgpd(&b).is_none_or(|d| d.descriptions.len() <= b.len()));
                assert!(parse_sbgp(&b).is_none_or(|s| s.runs.len() <= b.len()));
                for cut in 0..b.len() {
                    let _ = (parse_sgpd(&b[..cut]), parse_sbgp(&b[..cut]));
                }
            }
        }
    }

    #[test]
    fn fragment_indices_name_the_movie_or_the_fragment() {
        let sgpd = |n: u8| {
            [
                &[1, 0, 0, 0][..],
                b"tscl",
                &[0, 0, 0, 1],
                &[0, 0, 0, 1],
                &[n],
            ]
            .concat()
        };
        let sbgp = |runs: &[(u32, u32)]| {
            let mut b = vec![0u8; 4];
            b.extend_from_slice(b"tscl");
            b.extend_from_slice(&(runs.len() as u32).to_be_bytes());
            for (n, i) in runs {
                b.extend_from_slice(&n.to_be_bytes());
                b.extend_from_slice(&i.to_be_bytes());
            }
            b
        };
        let (sg, sb) = (sgpd(1), sbgp(&[(2, 1), (1, 0), (1, 2)]));
        let mut t = GroupTable::from_stbl(&[(*b"sgpd", &sg), (*b"sbgp", &sb)], 0);
        let (fg, fb) = (sgpd(5), sbgp(&[(1, 1), (1, 0x1_0001), (9, 0x1_0002)]));
        t.add_fragment(&[(*b"sgpd", &fg), (*b"sbgp", &fb)], 4, 3);
        let g = t.finish().remove(0);
        assert_eq!(g.descriptions, [[1], [5]]);
        let of: Vec<_> = (0..8).map(|i| g.group_of(i)).collect();
        let want = [Some(0), Some(0), None, None, Some(0), Some(1), None, None];
        assert_eq!(of, want);
    }

    #[test]
    fn many_groupings_are_limited() {
        let sbgp: Vec<Vec<u8>> = (0..1000u32)
            .map(|i| {
                let mut b = vec![1u8, 0, 0, 0];
                b.extend_from_slice(b"roll");
                b.extend_from_slice(&i.to_be_bytes());
                b.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1]);
                b
            })
            .collect();
        let kids: Vec<([u8; 4], &[u8])> = sbgp.iter().map(|b| (*b"sbgp", &b[..])).collect();
        let t = GroupTable::from_stbl(&kids, 0);
        assert_eq!(t.finish().len(), MAX_GROUPINGS);
    }
}
