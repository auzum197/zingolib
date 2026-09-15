fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .build_server(true)
        .compile_protos(
            &[
                "../zingolib-testutils/proto/compact_formats.proto",
                "proto/darkside.proto",
                "../zingolib-testutils/proto/service.proto",
            ],
            &["proto", "../zingolib-testutils/proto"],
        )?;
    println!("cargo:rerun-if-changed=proto/darkside.proto");
    Ok(())
}
