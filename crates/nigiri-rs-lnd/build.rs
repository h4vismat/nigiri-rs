fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    unsafe { std::env::set_var("PROTOC", protoc) };
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(false)
        .compile_protos(
            &[
                "proto/lightning.proto",
                "proto/walletunlocker.proto",
                "proto/routerrpc/router.proto",
            ],
            &["proto"],
        )?;
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&["proto/test/harness.proto"], &["proto/test"])?;
    println!("cargo:rerun-if-changed=proto");
    Ok(())
}
