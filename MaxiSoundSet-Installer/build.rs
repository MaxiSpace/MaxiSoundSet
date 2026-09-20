fn main() {
    let app = std::fs::read_to_string("../MaxiSoundSet-Rust/Cargo.toml").unwrap();
    let version = app
        .lines()
        .find(|s| s.starts_with("version = "))
        .unwrap()
        .split('"')
        .nth(1)
        .unwrap();
    println!("cargo:rustc-env=MAXI_APP_VERSION={version}");
    let resource = std::fs::read_to_string("../MaxiSoundSet-Rust/assets/app.rc")
        .unwrap()
        .replace(
            "\"MAXI SOUNDSET\\0\"",
            "\"MAXI SOUNDSET Installer / Uninstaller\\0\"",
        )
        .replace("MaxiSoundSet.exe", "MaxiSoundSet-Setup.exe");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(out.join("setup.rc"), resource).unwrap();
    slint_build::compile_with_config(
        "ui/installer.slint",
        slint_build::CompilerConfiguration::new().with_style("fluent-dark".into()),
    )
    .unwrap();
    embed_resource::compile(out.join("setup.rc"), embed_resource::NONE)
        .manifest_required()
        .unwrap();
    for p in [
        "../MaxiSoundSet-Rust/Cargo.toml",
        "../MaxiSoundSet-Rust/assets/app.rc",
        "../MaxiSoundSet-Rust/MaxiSoundSet.exe",
        "../MaxiSoundSet-Rust/LICENSE.txt",
        "assets/EULA_FA.md",
        "assets/EULA_EN.md",
        "assets/icon.ico",
        "assets/app.manifest",
    ] {
        println!("cargo:rerun-if-changed={p}");
    }
}
