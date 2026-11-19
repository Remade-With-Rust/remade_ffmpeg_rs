//! Hand-declared libtiff 4.x prototypes (no headers on the box) and an
//! in-memory client so libtiff reads and writes RAM, like the Rust arm.
#![allow(non_camel_case_types, clippy::missing_safety_doc)]

use std::ffi::{c_char, c_int, c_uint, c_void, CString};

pub type TIFF = c_void;
pub type tmsize_t = isize;
pub type toff_t = u64;
type ReadWrite = extern "C" fn(*mut c_void, *mut c_void, tmsize_t) -> tmsize_t;
type SeekProc = extern "C" fn(*mut c_void, toff_t, c_int) -> toff_t;
type CloseProc = extern "C" fn(*mut c_void) -> c_int;
type SizeProc = extern "C" fn(*mut c_void) -> toff_t;
type MapProc = extern "C" fn(*mut c_void, *mut *mut c_void, *mut toff_t) -> c_int;
type UnmapProc = extern "C" fn(*mut c_void, *mut c_void, toff_t);
type Handler = Option<extern "C" fn(*const c_char, *const c_char, *mut c_void)>;

extern "C" {
    pub fn TIFFClientOpen(
        name: *const c_char,
        mode: *const c_char,
        h: *mut c_void,
        r: ReadWrite,
        w: ReadWrite,
        s: SeekProc,
        c: CloseProc,
        z: SizeProc,
        m: MapProc,
        u: UnmapProc,
    ) -> *mut TIFF;
    pub fn TIFFClose(t: *mut TIFF);
    pub fn TIFFGetField(t: *mut TIFF, tag: c_uint, ...) -> c_int;
    pub fn TIFFGetFieldDefaulted(t: *mut TIFF, tag: c_uint, ...) -> c_int;
    pub fn TIFFSetField(t: *mut TIFF, tag: c_uint, ...) -> c_int;
    pub fn TIFFIsTiled(t: *mut TIFF) -> c_int;
    pub fn TIFFNumberOfStrips(t: *mut TIFF) -> u32;
    pub fn TIFFNumberOfTiles(t: *mut TIFF) -> u32;
    pub fn TIFFStripSize(t: *mut TIFF) -> tmsize_t;
    pub fn TIFFTileSize(t: *mut TIFF) -> tmsize_t;
    pub fn TIFFReadEncodedStrip(t: *mut TIFF, s: u32, buf: *mut c_void, n: tmsize_t) -> tmsize_t;
    pub fn TIFFReadEncodedTile(t: *mut TIFF, s: u32, buf: *mut c_void, n: tmsize_t) -> tmsize_t;
    pub fn TIFFWriteEncodedStrip(t: *mut TIFF, s: u32, buf: *mut c_void, n: tmsize_t) -> tmsize_t;
    pub fn TIFFWriteDirectory(t: *mut TIFF) -> c_int;
    pub fn TIFFReadDirectory(t: *mut TIFF) -> c_int;
    pub fn TIFFSetErrorHandler(h: Handler) -> Handler;
    pub fn TIFFSetWarningHandler(h: Handler) -> Handler;
    pub fn TIFFGetVersion() -> *const c_char;
    pub fn clock_gettime(clk: c_int, ts: *mut [i64; 2]) -> c_int;
}

pub const CLOCK_THREAD_CPUTIME_ID: c_int = 3;

/// Thread CPU time in seconds.
pub fn thread_cpu() -> f64 {
    let mut ts = [0i64; 2];
    // SAFETY: valid out-pointer to a timespec-sized array.
    unsafe { clock_gettime(CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts[0] as f64 + ts[1] as f64 * 1e-9
}

pub fn quiet() {
    // SAFETY: passing NULL handlers disables libtiff's stderr messages.
    unsafe {
        TIFFSetErrorHandler(None);
        TIFFSetWarningHandler(None);
    }
}

pub fn version() -> String {
    // SAFETY: libtiff returns a static NUL-terminated string.
    unsafe { std::ffi::CStr::from_ptr(TIFFGetVersion()) }
        .to_string_lossy()
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

/// In-memory file for libtiff: reads from `data`, writes grow it.
pub struct Mem {
    pub data: Vec<u8>,
    pub pos: u64,
}

extern "C" fn mem_read(h: *mut c_void, buf: *mut c_void, n: tmsize_t) -> tmsize_t {
    // SAFETY: `h` is the &mut Mem passed to TIFFClientOpen, alive until close.
    let m = unsafe { &mut *h.cast::<Mem>() };
    let start = (m.pos as usize).min(m.data.len());
    let k = (n.max(0) as usize).min(m.data.len() - start);
    // SAFETY: libtiff provides a buffer of at least n bytes.
    unsafe { std::ptr::copy_nonoverlapping(m.data.as_ptr().add(start), buf.cast::<u8>(), k) };
    m.pos += k as u64;
    k as tmsize_t
}
extern "C" fn mem_write(h: *mut c_void, buf: *mut c_void, n: tmsize_t) -> tmsize_t {
    // SAFETY: as above.
    let m = unsafe { &mut *h.cast::<Mem>() };
    let n = n.max(0) as usize;
    let end = m.pos as usize + n;
    if m.data.len() < end {
        m.data.resize(end, 0);
    }
    // SAFETY: libtiff provides n readable bytes.
    let src = unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), n) };
    m.data[m.pos as usize..end].copy_from_slice(src);
    m.pos = end as u64;
    n as tmsize_t
}
extern "C" fn mem_seek(h: *mut c_void, off: toff_t, whence: c_int) -> toff_t {
    // SAFETY: as above.
    let m = unsafe { &mut *h.cast::<Mem>() };
    m.pos = match whence {
        0 => off,
        1 => m.pos.wrapping_add(off),
        _ => (m.data.len() as u64).wrapping_add(off),
    };
    m.pos
}
extern "C" fn mem_close(_: *mut c_void) -> c_int {
    0
}
extern "C" fn mem_size(h: *mut c_void) -> toff_t {
    // SAFETY: as above.
    unsafe { &*h.cast::<Mem>() }.data.len() as toff_t
}
extern "C" fn mem_map(h: *mut c_void, base: *mut *mut c_void, size: *mut toff_t) -> c_int {
    // SAFETY: as above; libtiff only reads through a read-mode mapping.
    let m = unsafe { &mut *h.cast::<Mem>() };
    unsafe {
        *base = m.data.as_mut_ptr().cast();
        *size = m.data.len() as toff_t;
    }
    1
}
extern "C" fn mem_unmap(_: *mut c_void, _: *mut c_void, _: toff_t) {}

/// Open `m` with libtiff. Mode "r" maps the buffer (libtiff's normal file
/// fast path), "w" writes into it. The caller must keep `m` alive and not
/// move it until `TIFFClose`.
pub fn open(m: &mut Mem, mode: &str) -> *mut TIFF {
    let name = CString::new("mem").unwrap();
    let mode = CString::new(mode).unwrap();
    // SAFETY: callbacks match libtiff's typedefs; `m` outlives the handle.
    unsafe {
        TIFFClientOpen(
            name.as_ptr(),
            mode.as_ptr(),
            (m as *mut Mem).cast(),
            mem_read,
            mem_write,
            mem_seek,
            mem_close,
            mem_size,
            mem_map,
            mem_unmap,
        )
    }
}

pub fn get_u16(t: *mut TIFF, tag: u32, default: bool) -> Option<u16> {
    let mut v: u16 = 0;
    // SAFETY: these tags are documented as uint16* out-parameters.
    let ok = unsafe {
        if default {
            TIFFGetFieldDefaulted(t, tag, &mut v as *mut u16)
        } else {
            TIFFGetField(t, tag, &mut v as *mut u16)
        }
    };
    (ok == 1).then_some(v)
}

pub fn get_u32(t: *mut TIFF, tag: u32, default: bool) -> Option<u32> {
    let mut v: u32 = 0;
    // SAFETY: these tags are documented as uint32* out-parameters.
    let ok = unsafe {
        if default {
            TIFFGetFieldDefaulted(t, tag, &mut v as *mut u32)
        } else {
            TIFFGetField(t, tag, &mut v as *mut u32)
        }
    };
    (ok == 1).then_some(v)
}
