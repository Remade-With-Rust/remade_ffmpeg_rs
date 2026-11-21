fn main() {
    let pc = std::process::Command::new("pkg-config")
        .args(["--cflags", "cairo"])
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
    println!("cargo:rustc-link-lib=cairo");
    println!("cargo:rustc-link-lib=m");
    println!("cargo:rerun-if-changed=shim.c");
}
