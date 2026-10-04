use std::process::Command;

#[test]
fn server_reports_compiled_component_without_config_or_host_setup() {
    let output = Command::new(env!("CARGO_BIN_EXE_server"))
        .args([
            "--component-info-json",
            "--config",
            "/missing/m3/config.toml",
        ])
        .env_clear()
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let info: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(info["backend"], agentenv::sandbox::selected::BACKEND);
    assert_eq!(
        info["firecracker"],
        cfg!(feature = "hypervisor-firecracker")
    );
    assert_eq!(info["dragonball"], cfg!(feature = "hypervisor-dragonball"));
    assert_eq!(info["debug_assertions"], cfg!(debug_assertions));
    assert_eq!(info["architecture"], std::env::consts::ARCH);
    assert_eq!(info["os"], std::env::consts::OS);
    assert_eq!(info["version"], env!("CARGO_PKG_VERSION"));
    assert!(info["git_commit"].as_str().is_some());
    assert!(output.stderr.is_empty());
}
