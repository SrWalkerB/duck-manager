use std::{env, path::PathBuf, process::Command};

fn main() {
    let output = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set by Cargo"))
        .join("duck-packages.gresource");
    let status = Command::new("glib-compile-resources")
        .args([
            "src/duck-packages.gresource.xml",
            "--sourcedir=data",
            "--target",
        ])
        .arg(&output)
        .status()
        .expect("glib-compile-resources must be installed");
    assert!(status.success(), "failed to compile application resources");
    println!("cargo:rerun-if-changed=src/duck-packages.gresource.xml");
    println!("cargo:rerun-if-changed=data/style.css");
}
