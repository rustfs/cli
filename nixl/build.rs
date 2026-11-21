fn main() {
    #[cfg(feature = "cuobject")]
    build_cuobject();
}

#[cfg(feature = "cuobject")]
fn build_cuobject() {
    use std::{env, path::PathBuf};

    for name in ["CUOBJCLIENT_ROOT", "CUDA_HOME"] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    for file in [
        "src/nvidia/sdk.rs",
        "native/cuobject.h",
        "native/cuobject.cc",
    ] {
        println!("cargo:rerun-if-changed={file}");
    }
    let cuda = env::var_os("CUDA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/local/cuda"));
    let sdk = env::var_os("CUOBJCLIENT_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| cuda.clone());
    cxx_build::bridge("src/nvidia/sdk.rs")
        .file("native/cuobject.cc")
        .include(".")
        .include(sdk.join("include"))
        .include(cuda.join("include"))
        .std("c++17")
        .compile("rc_cuobject");
    for prefix in [&sdk, &cuda] {
        for dir in ["lib", "lib64"] {
            println!(
                "cargo:rustc-link-search=native={}",
                prefix.join(dir).display()
            );
        }
    }
    println!("cargo:rustc-link-lib=cuobjclient");
    println!("cargo:rustc-link-lib=cufile");
}
