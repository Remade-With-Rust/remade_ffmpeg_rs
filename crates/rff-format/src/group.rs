//! Sample groups and track references: what [`SampleIndex::sample_groups`]
//! and [`Demuxer::track_references`] give.
//!
//! [`SampleIndex::sample_groups`]: crate::SampleIndex::sample_groups
//! [`Demuxer::track_references`]: crate::Demuxer::track_references

/// One grouping of a stream's samples (MP4 `sbgp` + `sgpd`): the descriptions
/// of its groups, and which samples are in which.
///
/// Samples are numbered from 0 in the **stored** sample table, the numbering
/// of [`raw_sample_info`](crate::SampleIndex::raw_sample_info), fragments
/// included. (Uncompressed audio whose index has one entry per chunk is the
/// exception: groups then count `stsz` samples, i.e. PCM frames, as the file
/// does.)
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SampleGrouping {
    /// `grouping_type`: `roll`, `rap `, `seig`, `sync`, `tscl`, ...
    pub grouping_type: [u8; 4],
    /// `grouping_type_parameter`, when the `sbgp` is of version 1.
    pub parameter: Option<u32>,
    /// The group descriptions as stored (the payload of each `sgpd` entry).
    /// A fragment's own descriptions follow the movie's.
    pub descriptions: Vec<Vec<u8>>,
    /// The runs of samples the file assigns, in sample order.
    pub runs: Vec<SampleGroupRun>,
    /// The group of samples no run covers (`sgpd` version 2's
    /// `default_group_description_index`), as an index into `descriptions`.
    pub default: Option<usize>,
}

/// Consecutive samples with the same group assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct SampleGroupRun {
    pub first_sample: u64,
    pub sample_count: u32,
    /// Index into [`SampleGrouping::descriptions`], or `None` for samples
    /// the file puts in no group of this type.
    pub description: Option<usize>,
}

impl SampleGroupRun {
    pub fn new(first_sample: u64, sample_count: u32, description: Option<usize>) -> Self {
        SampleGroupRun {
            first_sample,
            sample_count,
            description,
        }
    }
}

impl SampleGrouping {
    pub fn new(grouping_type: [u8; 4], parameter: Option<u32>) -> SampleGrouping {
        SampleGrouping {
            grouping_type,
            parameter,
            descriptions: Vec::new(),
            runs: Vec::new(),
            default: None,
        }
    }

    /// The index in [`descriptions`](Self::descriptions) of stored sample
    /// `sample`'s group; `None` when it is in no group of this type.
    pub fn group_of(&self, sample: u64) -> Option<usize> {
        let n = self.runs.partition_point(|r| r.first_sample <= sample);
        match n.checked_sub(1).map(|i| &self.runs[i]) {
            Some(r) if sample - r.first_sample < u64::from(r.sample_count) => r.description,
            _ => self.default,
        }
    }

    /// The description of stored sample `sample`'s group.
    pub fn description_of(&self, sample: u64) -> Option<&[u8]> {
        self.descriptions
            .get(self.group_of(sample)?)
            .map(Vec::as_slice)
    }
}

/// A typed reference from one stream to others (MP4 `tref`): `tmcd` names the
/// timecode streams, `chap` the chapter track, `cdsc` described content, ...
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct TrackReference {
    pub kind: [u8; 4],
    /// Indices of the streams referred to.
    pub streams: Vec<usize>,
}

impl TrackReference {
    pub fn new(kind: [u8; 4], streams: Vec<usize>) -> TrackReference {
        TrackReference { kind, streams }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_outside_every_run_take_the_default_group() {
        let mut g = SampleGrouping::new(*b"roll", None);
        g.descriptions = vec![vec![1], vec![2]];
        g.runs = vec![
            SampleGroupRun::new(0, 2, Some(0)),
            SampleGroupRun::new(2, 1, None),
            SampleGroupRun::new(10, 1, Some(7)),
        ];
        assert_eq!(g.group_of(1), Some(0));
        assert_eq!(g.group_of(2), None);
        assert_eq!(g.group_of(3), None);
        g.default = Some(1);
        assert_eq!(g.description_of(3), Some(&[2u8][..]));
        assert_eq!(g.group_of(2), None); // explicitly in no group
        assert_eq!(g.description_of(10), None); // a dangling index
    }
}
