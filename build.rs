use embed_manifest::manifest::DpiAwareness;
use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_manifest(new_manifest("Clips.InstantReplay").dpi_awareness(DpiAwareness::PerMonitorV2))
            .expect("unable to embed manifest");
        embed_resource::compile("app.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("unable to embed icon resource");
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=app.ico");
}
