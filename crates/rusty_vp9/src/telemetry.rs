//! Entropy-bin telemetry for the private Prometheus refinery — the CASC
//! (context-adaptive symbolic coding) harvest tap.
//!
//! Records every **coefficient-token boolean bin** the encoder actually emits:
//! the packed site (node kind, coefficient band, coefficient context, tx
//! size), the coded bit, and the probability the shipping tables paid. The
//! tap lives on the emit path only (`BitSink for BoolEncoder` in
//! `encode::tokens`); RDO costing walks never record. It observes and never
//! alters coding decisions, so the bitstream is byte-identical with the
//! feature on or off — the tap exists to be *harvested*, not to change
//! behavior.
//!
//! Zero-cost when the `prometheus-telemetry` feature is off (this module
//! isn't compiled). When on but not [`enable`]d, the cost is one thread-local
//! branch per recorded bin.
//!
//! Driver contract (see `Prometheus/crates/prom-entropy`): call
//! `enable(true)`, push frames through [`crate::Vp9Encoder`], and drain
//! [`take`] after each frame — records accumulate per thread in stream order.

use std::cell::RefCell;

/// One recorded coefficient bin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoefBin {
    /// Packed site: `node | band<<3 | ctx<<6 | tx<<9`. See [`Site`].
    pub site: u16,
    /// The coded bit (0/1).
    pub bit: u8,
    /// The probability of **bit == 0** the coder used, on the u8 grid
    /// (`p ≈ prob/256`, `1..=255`).
    pub prob: u8,
}

/// The unpacked form of [`CoefBin::site`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Site {
    /// Which decision this bin is: 0 = EOB flag, 1 = zero/non-zero flag,
    /// 2 = the ONE/TWO+ pivot (model prob), 3 = magnitude-tree/category bits
    /// (static Pareto/CAT probs), 4 = sign (static 128).
    pub node: u8,
    /// Coefficient band (0..=5). Meaningful for nodes 0–2.
    pub band: u8,
    /// Coefficient context (0..=5). Meaningful for nodes 0–2.
    pub ctx: u8,
    /// Transform size (0..=3 = 4x4..32x32).
    pub tx: u8,
}

impl CoefBin {
    /// Unpack the site fields.
    pub fn site(&self) -> Site {
        Site {
            node: (self.site & 0x7) as u8,
            band: ((self.site >> 3) & 0x7) as u8,
            ctx: ((self.site >> 6) & 0x7) as u8,
            tx: ((self.site >> 9) & 0x3) as u8,
        }
    }
}

struct TapState {
    enabled: bool,
    site: u16,
    buf: Vec<CoefBin>,
}

thread_local! {
    static TAP: RefCell<TapState> = const {
        RefCell::new(TapState {
            enabled: false,
            site: 0,
            buf: Vec::new(),
        })
    };
}

/// Turn recording on/off for this thread. Off also clears the buffer.
pub fn enable(on: bool) {
    TAP.with(|t| {
        let mut t = t.borrow_mut();
        t.enabled = on;
        if !on {
            t.buf = Vec::new();
        }
    });
}

/// Drain everything recorded on this thread since the last `take`, in exact
/// emit (stream) order.
pub fn take() -> Vec<CoefBin> {
    TAP.with(|t| std::mem::take(&mut t.borrow_mut().buf))
}

/// Tag the site of the next recorded bin(s). Called from the token walk with
/// (node, band, ctx, tx) in scope; cheap enough to sit on the shared
/// emit/cost path (a thread-local store).
#[inline]
pub(crate) fn set_site(node: u8, band: u8, ctx: u8, tx: u8) {
    let packed =
        (node as u16 & 0x7) | ((band as u16 & 0x7) << 3) | ((ctx as u16 & 0x7) << 6) | ((tx as u16 & 0x3) << 9);
    TAP.with(|t| t.borrow_mut().site = packed);
}

/// Record one emitted bin under the current site tag. Called ONLY from the
/// real `BoolEncoder` sink — costing sinks never reach here.
#[inline]
pub(crate) fn record(bit: u8, prob: u8) {
    TAP.with(|t| {
        let mut t = t.borrow_mut();
        if t.enabled {
            let site = t.site;
            t.buf.push(CoefBin { site, bit, prob });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_packs_and_unpacks() {
        set_site(2, 5, 3, 1);
        enable(true);
        record(1, 200);
        let v = take();
        enable(false);
        assert_eq!(v.len(), 1);
        let s = v[0].site();
        assert_eq!((s.node, s.band, s.ctx, s.tx), (2, 5, 3, 1));
        assert_eq!((v[0].bit, v[0].prob), (1, 200));
    }

    #[test]
    fn disabled_records_nothing() {
        enable(false);
        record(0, 128);
        assert!(take().is_empty());
    }
}
