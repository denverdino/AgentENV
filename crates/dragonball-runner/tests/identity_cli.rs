use std::process::Command;

#[test]
fn runtime_identity_cli_reports_reader_build_before_resource_allocation() {
    let output = Command::new(env!("CARGO_BIN_EXE_aenv-dragonball"))
        .args(["--runtime", "--version-json"])
        .env_clear()
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let identity: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        identity["dragonball_revision"],
        "9b0d2852e4a9d4c66a391c32fccb93b65e937f8f"
    );
    assert_eq!(identity["feature_profile"], "db-virtio-blk-net-balloon-v2");
    assert_eq!(identity["snapshot_format_epoch"], 2);
    assert!(identity["runner_build_id"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert!(
        identity.get("runner_instance_id").is_none(),
        "build query is not a runtime lease"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn identity_cli_reports_pinned_revision_without_kvm() {
    // A wrong revision/profile lets a node restore with an incompatible runner.
    let output = Command::new(env!("CARGO_BIN_EXE_aenv-dragonball"))
        .arg("--version-json")
        .env_clear()
        .output()
        .expect("run identity CLI without runtime configuration");
    assert!(output.status.success(), "{:?}", output);
    let identity: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        identity["dragonball_revision"],
        "9b0d2852e4a9d4c66a391c32fccb93b65e937f8f"
    );
    assert_eq!(identity["protocol_version"], 1);
    assert_eq!(identity["feature_profile"], "m0-mmio-blk-net");
    assert_eq!(identity["runner_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(identity.as_object().unwrap().len(), 4);
    assert!(output.stderr.is_empty());
}
