//! Puts the app icon into the Windows executable
//! (`packaging/windows/buzz.rc`). Other platforms take theirs from the
//! package.

fn main() {
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let resources = std::path::Path::new("../../packaging/windows");
        println!("cargo:rerun-if-changed={}", resources.display());
        embed_resource::compile(
            resources.join("buzz.rc"),
            embed_resource::ParamsIncludeDirs([resources]),
        )
        .manifest_optional()
        .expect("the Windows resources compile");
    }
}
