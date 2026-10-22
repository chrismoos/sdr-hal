fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "network-backend")]
    if std::env::var_os("CARGO_FEATURE_NETWORK_BACKEND").is_some() {
        tonic_build::configure()
            .build_server(true)
            .build_client(true)
            .compile_protos(&["proto/radio/v1/service.proto"], &["proto"])?;
    }
    Ok(())
}
