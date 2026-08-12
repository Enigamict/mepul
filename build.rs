//! Compiles the eBPF observer alongside the binary that loads it.
//!
//! The object is embedded in the executable rather than shipped beside it, so a
//! runner needs nothing installed to be observed — which is the point, given
//! that runners are wiped between jobs.

use std::path::PathBuf;
use std::process::Command;

const SOURCE: &str = "bpf/observe.bpf.c";

fn main() {
    println!("cargo:rerun-if-changed={SOURCE}");
    println!("cargo:rerun-if-changed=bpf/vmlinux.h");

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    let object = out.join("observe.bpf.o");

    // BPF is little-endian on every platform this targets, but the tracepoint
    // argument layout differs between architectures, so the target arch is
    // passed through to the program.
    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok("x86_64") => "x86",
        Ok(other) => panic!("no BPF observer for {other}"),
        Err(e) => panic!("could not read the target architecture: {e}"),
    };

    let status = Command::new("clang")
        .args(["-target", "bpf"])
        .arg(format!("-D__TARGET_ARCH_{arch}"))
        .args(["-O2", "-g", "-Wall", "-Werror"])
        .args(["-I", "bpf"])
        .arg("-c")
        .arg(SOURCE)
        .arg("-o")
        .arg(&object)
        .status()
        .unwrap_or_else(|e| panic!("failed to run clang (is it installed?): {e}"));

    if !status.success() {
        panic!("clang failed to build {SOURCE}");
    }

    println!("cargo:rustc-env=MEPUL_BPF_OBJECT={}", object.display());
}
