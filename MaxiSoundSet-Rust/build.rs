fn main() {
    let compiler =
        std::process::Command::new(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into()))
            .arg("--version")
            .output()
            .expect("Cannot read Rust compiler version");
    let compiler = String::from_utf8(compiler.stdout).expect("Rust version is not UTF-8");
    println!("cargo:rustc-env=MAXI_RUST_VERSION={}", compiler.trim());
    let lock = std::fs::read_to_string("Cargo.lock").expect("Cargo.lock is required");
    let mut packages = Vec::new();
    for block in lock.split("[[package]]").skip(1) {
        let value = |key: &str| {
            block.lines().find_map(|line| {
                line.strip_prefix(key)
                    .and_then(|v| v.strip_suffix('"'))
                    .map(str::to_owned)
            })
        };
        if let (Some(name), Some(version)) = (value("name = \""), value("version = \"")) {
            packages.push(format!("{name}  ·  {version}"));
        }
    }
    packages.sort();
    packages.dedup();
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(out.join("dependencies.txt"), packages.join("\n"))
        .expect("Cannot record dependency versions");
    println!("cargo:rerun-if-changed=Cargo.lock");
    slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new().with_style("fluent-dark".into()),
    )
    .expect("Slint UI compilation failed");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("assets/app.rc", embed_resource::NONE)
            .manifest_required()
            .unwrap();
    }
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/app.manifest");
    println!("cargo:rerun-if-changed=assets/icon.ico");
}
