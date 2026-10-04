fn main() {
    use sha2::{Digest, Sha256};
    println!("cargo:rerun-if-changed=Cargo.toml");
    let manifest = std::fs::read_to_string("Cargo.toml").expect("read runner manifest");
    let dependency = manifest
        .lines()
        .find(|line| line.starts_with("dragonball = "))
        .expect("pinned Dragonball dependency");
    let revision = dependency
        .split("rev = \"")
        .nth(1)
        .and_then(|value| value.split('"').next())
        .expect("Dragonball revision");
    assert!(revision.len() == 40 && revision.bytes().all(|b| b.is_ascii_hexdigit()));
    println!("cargo:rustc-env=DRAGONBALL_REVISION={revision}");
    let mut files = vec![
        std::path::PathBuf::from("build.rs"),
        std::path::PathBuf::from("Cargo.toml"),
        std::path::PathBuf::from("../../Cargo.toml"),
        std::path::PathBuf::from("../../Cargo.lock"),
        std::path::PathBuf::from("../vmm-protocol/Cargo.toml"),
    ];
    for directory in ["src", "../vmm-protocol/src"] {
        println!("cargo:rerun-if-changed={directory}");
        for entry in std::fs::read_dir(directory).expect("read runner/protocol source directory") {
            let path = entry.expect("read source entry").path();
            if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    let mut digest = Sha256::new();
    for file in files {
        println!("cargo:rerun-if-changed={}", file.display());
        let contents = std::fs::read(&file).expect("read build fingerprint input");
        digest.update(file.to_string_lossy().as_bytes());
        digest.update([0]);
        digest.update((contents.len() as u64).to_le_bytes());
        digest.update(contents);
    }
    let compiler =
        std::process::Command::new(std::env::var_os("RUSTC").expect("Cargo supplies RUSTC"))
            .arg("-Vv")
            .output()
            .expect("read compiler identity");
    assert!(compiler.status.success(), "read compiler identity failed");
    digest.update(compiler.stdout);
    for name in [
        "CARGO_CFG_TARGET_ARCH",
        "CARGO_CFG_TARGET_OS",
        "CARGO_CFG_TARGET_ENV",
        "CARGO_CFG_TARGET_FEATURE",
        "CARGO_ENCODED_RUSTFLAGS",
        "PROFILE",
        "OPT_LEVEL",
        "DEBUG",
        "TARGET",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
        digest.update(name.as_bytes());
        digest.update([0]);
        digest.update(std::env::var(name).unwrap_or_default().as_bytes());
        digest.update([0]);
    }
    println!(
        "cargo:rustc-env=RUNTIME_BUILD_ID=sha256:{:x}",
        digest.finalize()
    );
}
