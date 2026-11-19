//! Minimal FFI to the system LittleCMS 2 (the oracle). Format and flag
//! values are the ones `lcms2.h` 2.17 defines (printed by a C probe).

use std::ffi::c_void;

pub const TYPE_RGB_8: u32 = 262_169;
pub const TYPE_RGB_16: u32 = 262_170;
pub const TYPE_RGB_FLT: u32 = 4_456_476;
pub const TYPE_RGB_DBL: u32 = 4_456_472;
pub const TYPE_CMYK_8: u32 = 393_249;
pub const TYPE_CMYK_16: u32 = 393_250;
pub const TYPE_CMYK_FLT: u32 = 4_587_556;
pub const TYPE_CMYK_DBL: u32 = 4_587_552;
pub const TYPE_GRAY_8: u32 = 196_617;
pub const TYPE_GRAY_16: u32 = 196_618;
pub const TYPE_GRAY_FLT: u32 = 4_390_924;
pub const TYPE_GRAY_DBL: u32 = 4_390_920;
pub const TYPE_LAB_8: u32 = 655_385;
pub const TYPE_LAB_16: u32 = 655_386;
pub const TYPE_LAB_FLT: u32 = 4_849_692;
pub const TYPE_LAB_DBL: u32 = 4_849_688;

pub const FLAGS_BPC: u32 = 8192;
pub const FLAGS_NOCACHE: u32 = 64;
pub const FLAGS_NOOPTIMIZE: u32 = 256;
pub const FLAGS_HIGHRESPRECALC: u32 = 1024;
pub const FLAGS_SOFTPROOFING: u32 = 16384;

#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
pub struct CieXyz {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

#[repr(C)]
pub struct Timespec {
    pub sec: i64,
    pub nsec: i64,
}

extern "C" {
    fn cmsOpenProfileFromMem(mem: *const c_void, size: u32) -> *mut c_void;
    fn cmsCreateLab4Profile(wp: *const c_void) -> *mut c_void;
    fn cmsCloseProfile(p: *mut c_void) -> i32;
    fn cmsCreateTransform(
        i: *mut c_void,
        fin: u32,
        o: *mut c_void,
        fout: u32,
        intent: u32,
        flags: u32,
    ) -> *mut c_void;
    fn cmsCreateProofingTransform(
        i: *mut c_void,
        fin: u32,
        o: *mut c_void,
        fout: u32,
        proof: *mut c_void,
        intent: u32,
        proof_intent: u32,
        flags: u32,
    ) -> *mut c_void;
    fn cmsDoTransform(t: *mut c_void, inp: *const c_void, out: *mut c_void, n: u32);
    fn cmsDeleteTransform(t: *mut c_void);
    fn cmsDetectBlackPoint(bp: *mut CieXyz, p: *mut c_void, intent: u32, flags: u32) -> i32;
    fn cmsDetectDestinationBlackPoint(
        bp: *mut CieXyz,
        p: *mut c_void,
        intent: u32,
        flags: u32,
    ) -> i32;
    fn clock_gettime(clock: i32, ts: *mut Timespec) -> i32;
}

/// Thread CPU time in seconds (`CLOCK_THREAD_CPUTIME_ID`).
pub fn thread_cpu() -> f64 {
    let mut ts = Timespec { sec: 0, nsec: 0 };
    // SAFETY: valid out-pointer; clock id 3 is CLOCK_THREAD_CPUTIME_ID.
    unsafe { clock_gettime(3, &mut ts) };
    ts.sec as f64 + ts.nsec as f64 * 1e-9
}

pub struct Prof(*mut c_void);

impl Prof {
    pub fn from_mem(b: &[u8]) -> Self {
        // SAFETY: lcms copies what it needs from the buffer during the call.
        let p = unsafe { cmsOpenProfileFromMem(b.as_ptr().cast(), b.len() as u32) };
        assert!(!p.is_null(), "lcms2 rejected profile");
        Self(p)
    }
    pub fn lab4() -> Self {
        // SAFETY: NULL white point means D50.
        let p = unsafe { cmsCreateLab4Profile(std::ptr::null()) };
        assert!(!p.is_null());
        Self(p)
    }
    pub fn black_point(&self, intent: u32) -> CieXyz {
        let mut b = CieXyz::default();
        // SAFETY: valid handle and out-pointer.
        unsafe { cmsDetectBlackPoint(&mut b, self.0, intent, 0) };
        b
    }
    pub fn dest_black_point(&self, intent: u32) -> CieXyz {
        let mut b = CieXyz::default();
        // SAFETY: valid handle and out-pointer.
        unsafe { cmsDetectDestinationBlackPoint(&mut b, self.0, intent, 0) };
        b
    }
}

impl Drop for Prof {
    fn drop(&mut self) {
        // SAFETY: handle from cmsOpenProfileFromMem/cmsCreateLab4Profile.
        unsafe { cmsCloseProfile(self.0) };
    }
}

pub struct Xf {
    h: *mut c_void,
    in_bytes_per_px: usize,
    out_bytes_per_px: usize,
}

fn bytes_per_px(fmt: u32) -> usize {
    let ch = ((fmt >> 3) & 15) as usize;
    let b = (fmt & 7) as usize;
    ch * if b == 0 { 8 } else { b }
}

impl Xf {
    pub fn new(src: &Prof, fin: u32, dst: &Prof, fout: u32, intent: u32, flags: u32) -> Self {
        // SAFETY: valid handles; lcms validates the formats.
        let h = unsafe { cmsCreateTransform(src.0, fin, dst.0, fout, intent, flags) };
        assert!(!h.is_null(), "lcms2 could not create transform");
        Self {
            h,
            in_bytes_per_px: bytes_per_px(fin),
            out_bytes_per_px: bytes_per_px(fout),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn proofing(
        src: &Prof,
        fin: u32,
        dst: &Prof,
        fout: u32,
        proof: &Prof,
        intent: u32,
        proof_intent: u32,
        flags: u32,
    ) -> Self {
        // SAFETY: valid handles.
        let h = unsafe {
            cmsCreateProofingTransform(
                src.0,
                fin,
                dst.0,
                fout,
                proof.0,
                intent,
                proof_intent,
                flags | FLAGS_SOFTPROOFING,
            )
        };
        assert!(!h.is_null(), "lcms2 could not create proofing transform");
        Self {
            h,
            in_bytes_per_px: bytes_per_px(fin),
            out_bytes_per_px: bytes_per_px(fout),
        }
    }

    /// Transforms `src` into `dst`; panics unless both hold the same pixel
    /// count for the formats this transform was built with.
    pub fn run<A: Copy, B: Copy>(&self, src: &[A], dst: &mut [B]) {
        let sb = std::mem::size_of_val(src);
        let db = std::mem::size_of_val(dst);
        let n = sb / self.in_bytes_per_px;
        assert_eq!(sb, n * self.in_bytes_per_px);
        assert_eq!(db, n * self.out_bytes_per_px, "output size mismatch");
        // SAFETY: buffer sizes were checked against the formats above.
        unsafe {
            cmsDoTransform(
                self.h,
                src.as_ptr().cast(),
                dst.as_mut_ptr().cast(),
                n as u32,
            )
        };
    }
}

impl Drop for Xf {
    fn drop(&mut self) {
        // SAFETY: handle from cmsCreate*Transform.
        unsafe { cmsDeleteTransform(self.h) };
    }
}
