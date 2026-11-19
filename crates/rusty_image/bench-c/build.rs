fn main() {
    let pc = std::process::Command::new("pkg-config")
        .args(["--cflags", "gdk-pixbuf-2.0"])
        .output()
        .expect("pkg-config");
    let mut b = cc::Build::new();
    b.file("shim.c").include("third_party").opt_level(3);
    for f in String::from_utf8(pc.stdout).unwrap().split_whitespace() {
        if let Some(inc) = f.strip_prefix("-I") {
            b.include(inc);
        }
    }
    b.warnings(false).compile("shim");
    println!("cargo:rustc-link-lib=gdk_pixbuf-2.0");
    println!("cargo:rustc-link-lib=gobject-2.0");
    println!("cargo:rustc-link-lib=glib-2.0");
    println!("cargo:rustc-link-lib=dylib:+verbatim=libMagickWand-7.Q16.so.10");
    println!("cargo:rustc-link-lib=m");
    println!("cargo:rerun-if-changed=shim.c");
}
