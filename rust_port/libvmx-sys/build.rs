use std::env;
use std::path::PathBuf;

fn main() {
    let root_dir = PathBuf::from("../../libvmx/src");
    
    // 1. Generate Bindings
    let bindings = bindgen::Builder::default()
        .header(root_dir.join("vmxcodec.h").to_str().unwrap())
        .clang_arg("-x")
        .clang_arg("c++")
        .clang_arg("-std=c++17") // Ensure we use modern C++ standard
        .clang_arg("-fdeclspec") // Allow __declspec attributes
        .enable_cxx_namespaces() // Avoid name collisions
        .opaque_type("std::.*") // Treat STL types as opaque
        .allowlist_type("VMX_.*") // Whitelist VMX types to ensure they are generated
        .allowlist_function("VMX_.*") // Whitelist VMX functions
        .allowlist_var("VMX_.*") // Whitelist VMX constants
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("Unable to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Couldn't write bindings!");

    // 2. Compile C++ Library
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .include(&root_dir)
        .file(root_dir.join("vmxcodec.cpp"))
        //.file(root_dir.join("pch.cpp")) // Usually not needed if not using PCH in gcc/clang
        //.file(root_dir.join("dllmain.cpp")) // Windows specific usually
        .std("c++17") // Assuming C++17 or similar
        .flag("-w"); // Silence noisy third-party warnings (libvmx)

    // Architecture specific flags and files
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    println!("cargo:warning=TARGET_ARCH={}", target_arch);
    
    if target_arch == "x86_64" {
        build.flag("-mavx2");
        build.flag("-mfma"); // Often paired with AVX2
        build.file(root_dir.join("vmxcodec_avx2.cpp"));
        build.file(root_dir.join("vmxcodec_x86.cpp"));
    } else if target_arch == "aarch64" {
        // Neon is usually implicit on aarch64
        build.file(root_dir.join("vmxcodec_arm.cpp"));
    }

    // Handle OS specific includes if needed (e.g. windows vs linux)
    // For now assuming Mac/Linux environment from context.


    build.compile("vmxcodec");

    println!("cargo:rerun-if-changed=../../libvmx/src/vmxcodec.h");
    println!("cargo:rerun-if-changed=../../libvmx/src/vmxcodec.cpp");
}
