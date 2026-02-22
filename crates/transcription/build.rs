fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "remote-nim")]
    {
        tonic_build::configure()
            .build_server(false)
            .compile_protos(&["proto/riva_asr.proto"], &["proto/"])?;
    }
    Ok(())
}
