//! Compiles `proto/google/cloud/speech/v2/cloud_speech.proto` (a trimmed, wire-
//! compatible copy of Google's own proto — see that file's header comment) into a
//! tonic client via `tonic-prost-build`. Requires a `protoc` binary discoverable on
//! `PATH` (or pointed to via the `PROTOC` env var); this crate does not vendor one.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .build_server(false)
        .compile_protos(
            &["proto/google/cloud/speech/v2/cloud_speech.proto"],
            &["proto"],
        )?;
    Ok(())
}
