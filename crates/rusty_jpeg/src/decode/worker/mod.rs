mod immediate;
mod multithreaded;
#[cfg(all(not(target_arch = "wasm32"), feature = "rayon"))]
mod rayon;

use crate::decode::decoder::{choose_color_convert_func, ColorTransform};
use crate::decode::error::Result;
use crate::decode::parser::{Component, Dimensions};
use crate::decode::upsampler::Upsampler;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

pub struct RowData {
    pub index: usize,
    pub component: Component,
    pub quantization_table: Arc<[u16; 64]>,
    /// A previously-used output plane to refill instead of allocating.
    ///
    /// Sizing a fresh output plane measured **66.7% of whole-frame decode** —
    /// 3.1 MB per 1080p frame at an effective 522 MB/s, which is first-touch
    /// page-fault cost, not memset (touching the same pages a second time cost
    /// 1.2%). Handing back an already-faulted allocation removes it. FFmpeg
    /// solves the same problem with `AVBufferPool`.
    pub recycled: Option<Vec<u8>>,
}

pub trait Worker {
    fn start(&mut self, row_data: RowData) -> Result<()>;
    fn append_row(&mut self, row: (usize, Vec<i16>)) -> Result<()>;
    fn get_result(&mut self, index: usize) -> Result<Vec<u8>>;

    /// Hand back a coefficient buffer the worker has finished with, so the
    /// caller can refill it instead of allocating a new one.
    ///
    /// `decode_scan` otherwise allocates and zeroes a fresh `Vec<i16>` for every
    /// MCU row of every component — about 6.3 MB per 1080p frame. A worker that
    /// consumes rows synchronously can simply return the buffer; one that ships
    /// them to another thread cannot, so the default is `None` and the caller
    /// falls back to allocating.
    fn reclaim_buffer(&mut self) -> Option<Vec<i16>> {
        None
    }
    /// Default implementation for spawning multiple tasks.
    fn append_rows(&mut self, row: &mut dyn Iterator<Item = (usize, Vec<i16>)>) -> Result<()> {
        for item in row {
            self.append_row(item)?;
        }
        Ok(())
    }

    /// Concrete access to the synchronous worker, when this IS one.
    ///
    /// `fused_block` is called once per block -- ~49k times per 1080p frame --
    /// and through `&mut dyn Worker` every one of those is an indirect call that
    /// also blocks inlining of the transform dispatch into the decode loop.
    /// Resolving the concrete type ONCE per scan makes the whole fused body a
    /// static call the optimizer can see through.
    fn as_immediate(&mut self) -> Option<&mut immediate::ImmediateWorker> {
        None
    }

    /// Whether this worker can inverse-transform a block the moment it is
    /// decoded, instead of accumulating a whole MCU row of coefficients first.
    ///
    /// The row buffer costs a full write -> zero -> read round trip of **18.8 MB
    /// per 1080p frame**, through a ~60 KB-per-row buffer that does not fit in
    /// L1. It exists only so a row can be shipped to another THREAD; a worker
    /// that consumes blocks synchronously has no use for it and can keep the
    /// block in L1 from entropy decode straight through the IDCT.
    fn supports_fused(&self) -> bool {
        false
    }
}

#[allow(dead_code)]
/// Which worker the last decode actually instantiated.
///
/// `set_single_threaded` used to be silently ignored for baseline images: the
/// scan-loop call site hardcoded `Multithreaded`, and because the worker is
/// cached on first use it won. The flag looked live -- the profile barely moved
/// -- while the immediate worker, and with it `reclaim_buffer`, never ran.
/// This counter is what makes that observable from a test.
#[cfg(feature = "std")]
static LAST_WORKER_WAS_IMMEDIATE: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

#[cfg(feature = "std")]
fn record_worker_choice(prefer: &PreferWorkerKind) {
    LAST_WORKER_WAS_IMMEDIATE.store(
        matches!(prefer, PreferWorkerKind::Immediate),
        core::sync::atomic::Ordering::Relaxed,
    );
}

#[cfg(not(feature = "std"))]
fn record_worker_choice(_prefer: &PreferWorkerKind) {}

/// Whether the most recently created worker was the single-threaded one.
#[cfg(feature = "std")]
pub fn last_worker_was_immediate() -> bool {
    LAST_WORKER_WAS_IMMEDIATE.load(core::sync::atomic::Ordering::Relaxed)
}

#[derive(Debug)]
pub enum PreferWorkerKind {
    Immediate,
    Multithreaded,
}

#[derive(Default)]
pub struct WorkerScope {
    inner: core::cell::RefCell<Option<WorkerScopeInner>>,
}

// The immediate worker embeds its plane buffers while the threaded one holds
// channel handles, so the variants differ in size by design. Boxing the large
// variant would add an allocation to the path this crate spends most of its
// time in.
#[allow(clippy::large_enum_variant)]
enum WorkerScopeInner {
    #[cfg(all(not(target_arch = "wasm32"), feature = "rayon"))]
    Rayon(Box<rayon::Scoped>),
    #[cfg(not(target_arch = "wasm32"))]
    Multithreaded(multithreaded::MpscWorker),
    Immediate(immediate::ImmediateWorker),
}

impl WorkerScope {
    pub fn with<T>(with: impl FnOnce(&Self) -> T) -> T {
        with(&WorkerScope {
            inner: RefCell::default(),
        })
    }

    pub fn get_or_init_worker<T>(
        &self,
        prefer: PreferWorkerKind,
        f: impl FnOnce(&mut dyn Worker) -> T,
    ) -> T {
        let mut inner = self.inner.borrow_mut();
        if inner.is_none() {
            record_worker_choice(&prefer);
        }
        let inner = inner.get_or_insert_with(move || match prefer {
            #[cfg(all(not(target_arch = "wasm32"), feature = "rayon"))]
            PreferWorkerKind::Multithreaded => WorkerScopeInner::Rayon(Default::default()),
            #[allow(unreachable_patterns)]
            #[cfg(not(target_arch = "wasm32"))]
            PreferWorkerKind::Multithreaded => WorkerScopeInner::Multithreaded(Default::default()),
            _ => WorkerScopeInner::Immediate(Default::default()),
        });

        f(match &mut *inner {
            #[cfg(all(not(target_arch = "wasm32"), feature = "rayon"))]
            WorkerScopeInner::Rayon(worker) => worker.as_mut(),
            #[cfg(not(target_arch = "wasm32"))]
            WorkerScopeInner::Multithreaded(worker) => worker,
            WorkerScopeInner::Immediate(worker) => worker,
        })
    }
}

pub fn compute_image_parallel(
    components: &[Component],
    data: Vec<Vec<u8>>,
    output_size: Dimensions,
    color_transform: ColorTransform,
) -> Result<Vec<u8>> {
    #[cfg(all(not(target_arch = "wasm32"), feature = "rayon"))]
    return rayon::compute_image_parallel(components, data, output_size, color_transform);

    #[allow(unreachable_code)]
    {
        let color_convert_func = choose_color_convert_func(components.len(), color_transform)?;
        let upsampler = Upsampler::new(components, output_size.width, output_size.height)?;
        let line_size = output_size.width as usize * components.len();
        let mut image = vec![0u8; line_size * output_size.height as usize];

        for (row, line) in image.chunks_mut(line_size).enumerate() {
            upsampler.upsample_and_interleave_row(
                &data,
                row,
                output_size.width as usize,
                line,
                color_convert_func,
            );
        }

        Ok(image)
    }
}
