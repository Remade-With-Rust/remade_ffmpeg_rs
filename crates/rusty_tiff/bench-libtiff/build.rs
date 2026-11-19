// Links the system libtiff runtime. Ubuntu ships only libtiff.so.6 without
// the dev symlink or headers, so link it by file name; the prototypes are
// declared by hand in src/ffi.rs.
fn main() {
    println!("cargo:rustc-link-search=native=/usr/lib/x86_64-linux-gnu");
    println!("cargo:rustc-link-arg=-l:libtiff.so.6");
}
