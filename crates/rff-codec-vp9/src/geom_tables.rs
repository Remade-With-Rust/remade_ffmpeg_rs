//! VP9 block-geometry lookup tables - GENERATED from libvpx (BSD-3),
//! validated on extraction. BLOCK_INVALID=13, PARTITION order NHVS.
//! Index order: 4X4,4X8,8X4,8X8,8X16,16X8,16X16,16X32,32X16,32X32,32X64,64X32,64X64.

#![allow(dead_code)]

pub(crate) const B_WIDTH_LOG2: [u8; 13] = [0, 0, 1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4];
pub(crate) const B_HEIGHT_LOG2: [u8; 13] = [0, 1, 0, 1, 2, 1, 2, 3, 2, 3, 4, 3, 4];
pub(crate) const NUM_4X4_W: [u8; 13] = [1, 1, 2, 2, 2, 4, 4, 4, 8, 8, 8, 16, 16];
pub(crate) const NUM_4X4_H: [u8; 13] = [1, 2, 1, 2, 4, 2, 4, 8, 4, 8, 16, 8, 16];
pub(crate) const MI_WIDTH_LOG2: [u8; 13] = [0, 0, 0, 0, 0, 1, 1, 1, 2, 2, 2, 3, 3];
pub(crate) const NUM_8X8_W: [u8; 13] = [1, 1, 1, 1, 1, 2, 2, 2, 4, 4, 4, 8, 8];
pub(crate) const NUM_8X8_H: [u8; 13] = [1, 1, 1, 1, 2, 1, 2, 4, 2, 4, 8, 4, 8];
pub(crate) const SIZE_GROUP: [u8; 13] = [0, 0, 0, 1, 1, 1, 2, 2, 2, 3, 3, 3, 3];
pub(crate) const MAX_TXSIZE: [u8; 13] = [0, 0, 0, 1, 1, 1, 2, 2, 2, 3, 3, 3, 3];
pub(crate) const TX_MODE_TO_BIGGEST_TX: [u8; 5] = [0, 1, 2, 3, 3];
/// subsize_lookup[partition][block] -> BLOCK_SIZE (13 = invalid).
pub(crate) const SUBSIZE_LOOKUP: [[u8; 13]; 4] = [
  [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
  [13, 13, 13, 2, 13, 13, 5, 13, 13, 8, 13, 13, 11],
  [13, 13, 13, 1, 13, 13, 4, 13, 13, 7, 13, 13, 10],
  [13, 13, 13, 0, 13, 13, 3, 13, 13, 6, 13, 13, 9],
];
pub(crate) const PARTITION_CTX_ABOVE: [u8; 13] = [15, 15, 14, 14, 14, 12, 12, 12, 8, 8, 8, 0, 0];
pub(crate) const PARTITION_CTX_LEFT: [u8; 13] = [15, 14, 15, 14, 12, 14, 12, 8, 12, 8, 0, 8, 0];
