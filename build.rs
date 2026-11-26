fn main() {
    println!("cargo:rerun-if-changed=native/vz_bridge.h");
    println!("cargo:rerun-if-changed=native/vz_bridge.m");

    cc::Build::new()
        .file("native/vz_bridge.m")
        .include("native")
        .flag("-fobjc-arc")
        .flag("-Wno-deprecated-declarations")
        .compile("vz_bridge");

    println!("cargo:rustc-link-lib=framework=Foundation");
    println!("cargo:rustc-link-lib=framework=Virtualization");
    println!("cargo:rustc-link-lib=framework=AppKit");
}
