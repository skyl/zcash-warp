fn main() {
    tonic_build::configure()
        .out_dir("src/generated")
        .file_descriptor_set_path("cash.z.wallet.sdk.rpc.bin")
        .compile(
            &["proto/service.proto", "proto/compact_formats.proto"],
            &["proto"],
        )
        .unwrap();

    // create_c_bindings();
}

// #[allow(dead_code)]
// fn create_c_bindings() {
//     let crate_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
//     let config = cbindgen::Config::from_root_or_default(".");

//     cbindgen::Builder::new()
//         .with_crate(crate_dir)
//         .with_config(config)
//         .generate()
//         .expect("Unable to generate bindings")
//         .write_to_file("binding.h");
// }
