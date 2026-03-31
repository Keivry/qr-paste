// SPDX-License-Identifier: MIT OR Apache-2.0

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=../proto/relay.proto");
    println!("cargo:rerun-if-changed=assets/icon.ico");

    tonic_prost_build::compile_protos("../proto/relay.proto")?;

    if std::env::var_os("CARGO_CFG_TARGET_OS").as_deref() == Some(std::ffi::OsStr::new("windows")) {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/icon.ico");
        resource.compile()?;
    }

    Ok(())
}
