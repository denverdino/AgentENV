//! Offline paused-record inventory. Never invoke the server/persister GC here.
use anyhow::{ensure, Result};
use rocksdb::{IteratorMode, Options, DB};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::Path;

fn summarize(value: &[u8]) -> Value {
    let Ok(record) = serde_json::from_slice::<Value>(value) else {
        return json!({"classification":"unreadable-retain"});
    };
    let backend = record.get("backend").filter(|v| !v.is_null());
    let identity = record.get("restoreIdentity").filter(|v| !v.is_null());
    // Match the persisted reader's fallback and declaration agreement without
    // attempting to decode VM state. Missing legacy metadata backend means FC;
    // an explicitly malformed declaration must not get that default.
    let metadata_backend = record
        .pointer("/metadata/runtime_versions/backend")
        .map_or(Some("firecracker"), Value::as_str);
    let metadata_identity = record
        .pointer("/metadata/runtime_versions/restore_identity")
        .filter(|v| !v.is_null());
    let declarations_agree = backend.map_or(metadata_backend, Value::as_str) == metadata_backend
        && metadata_backend.is_some()
        && identity == metadata_identity;
    let classification = match (record["version"].as_u64(), backend) {
        (Some(1), None)
            if declarations_agree
                && metadata_backend == Some("firecracker")
                && identity.is_none() =>
        {
            "legacy-firecracker-retain"
        }
        (Some(1), Some(Value::String(b)))
            if declarations_agree && b == "firecracker" && identity.is_none() =>
        {
            "firecracker-retain"
        }
        (Some(1), Some(Value::String(b)))
            if declarations_agree
                && b == "dragonball"
                && identity.and_then(|i| i["backend"].as_str()) == Some("dragonball") =>
        {
            "dragonball-retain"
        }
        _ => "unknown-retain",
    };
    // Exact paths only: never walk user metadata, env or extension params.
    let mut dependencies = serde_json::Map::new();
    for pointer in [
        "/restoreIdentity/backend",
        "/restoreIdentity/architecture",
        "/restoreIdentity/virtualization_mode",
        "/restoreIdentity/runner_build_id",
        "/restoreIdentity/dragonball_revision",
        "/restoreIdentity/snapshot_format_epoch",
        "/restoreIdentity/restore_abi_id",
        "/restoreIdentity/cpu_policy_id",
        "/restoreIdentity/device_profile_id",
        "/restoreIdentity/memory_layout_id",
        "/restoreIdentity/kernel_version",
        "/restoreIdentity/tools_drive_version",
        "/restoreIdentity/envd_version",
        "/metadata/virtualization_mode",
        "/metadata/runtime_versions/backend",
        "/metadata/runtime_versions/vmm_version",
        "/metadata/runtime_versions/kernel_version",
        "/metadata/runtime_versions/firecracker_version",
        "/metadata/runtime_versions/tools_drive_version",
        "/metadata/runtime_versions/envd_version",
        "/state/common/firecracker_binary",
        "/state/common/tools_drive_version",
        "/state/common/envd_version",
    ] {
        if let Some(value) = record
            .pointer(pointer)
            .filter(|v| v.is_string() || v.is_number())
        {
            dependencies.insert(pointer.to_owned(), value.clone());
        }
    }
    json!({"classification": classification, "record_version": record["version"].as_u64(),
        "dependencies": dependencies})
}

fn inventory(path: &Path) -> Result<Value> {
    ensure!(
        path.is_dir() && path.join("CURRENT").is_file(),
        "expected an existing RocksDB directory"
    );
    let logs = tempfile::tempdir()?;
    let mut options = Options::default();
    options.create_if_missing(false);
    options.set_db_log_dir(logs.path());
    let db = DB::open_for_read_only(&options, path, false)?;
    let mut records = Vec::new();
    for item in db.iterator(IteratorMode::Start) {
        let (key, value) = item?;
        let mut record = summarize(&value);
        // Keys can also contain user data; report a stable digest, not raw bytes.
        record["key_sha256"] = json!(format!("{:x}", Sha256::digest(&key)));
        records.push(record);
    }
    Ok(
        json!({"count": records.len(), "records": records, "restore_qualified": false,
        "scope":"default column family; declared dependencies only, not restore validation"}),
    )
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    ensure!(args.len() == 1, "usage: recovery_inventory <records.db>");
    println!(
        "{}",
        serde_json::to_string_pretty(&inventory(Path::new(&args[0]))?)?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rocksdb::{Options, DB};
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::fs;

    fn files(path: &Path) -> BTreeMap<String, Vec<u8>> {
        fs::read_dir(path)
            .unwrap()
            .map(|item| {
                let item = item.unwrap();
                (
                    item.file_name().to_string_lossy().into_owned(),
                    fs::read(item.path()).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn missing_db_is_not_created() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("absent");
        assert!(inventory(&path).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn inventory_preserves_bytes_and_omits_private_metadata() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("records.db");
        {
            let mut opts = Options::default();
            opts.create_if_missing(true);
            let db = DB::open(&opts, &path).unwrap();
            db.put(b"legacy", serde_json::to_vec(&json!({"version":1,"state":{"common":{"firecracker_binary":"/retained/firecracker","tools_drive_version":"old-tools"}},"metadata":{"envVars":{"PASSWORD":"do-not-print"}}})).unwrap()).unwrap();
            let identity = json!({"backend":"dragonball","restore_abi_id":"abi-2","runner_build_id":"build-1"});
            db.put(b"db", serde_json::to_vec(&json!({"version":1,"backend":"dragonball","restoreIdentity":identity,"metadata":{"runtime_versions":{"backend":"dragonball","restore_identity":identity}}})).unwrap()).unwrap();
            db.put(
                b"future",
                br#"{"version":999,"metadata":{"secret":"do-not-print"}}"#,
            )
            .unwrap();
            db.put(b"corrupt", b"not json do-not-print").unwrap();
        }
        let before = files(&path);
        let report = inventory(&path).unwrap();
        assert_eq!(report["count"], 4);
        let records = report["records"].as_array().unwrap();
        assert_eq!(records[0]["classification"], "unreadable-retain");
        assert_eq!(records[1]["classification"], "dragonball-retain");
        assert_eq!(
            records[1]["dependencies"]["/restoreIdentity/restore_abi_id"],
            "abi-2"
        );
        assert_eq!(records[2]["classification"], "unknown-retain");
        assert_eq!(records[3]["classification"], "legacy-firecracker-retain");
        assert_eq!(
            records[3]["dependencies"]["/state/common/tools_drive_version"],
            "old-tools"
        );
        assert!(!report.to_string().contains("do-not-print"));
        assert_eq!(report["restore_qualified"], false);
        assert_eq!(files(&path), before);
    }

    #[test]
    fn ambiguous_records_are_not_classified_as_legacy() {
        for record in [
            json!({"version":999}),
            json!({"version":1,"backend":"future"}),
            json!({"version":1,"backend":"firecracker","restoreIdentity":{"backend":"dragonball"}}),
            json!({"version":1,"restoreIdentity":{"backend":"dragonball"}}),
            json!({"version":1,"backend":"dragonball"}),
        ] {
            assert_eq!(
                summarize(&serde_json::to_vec(&record).unwrap())["classification"],
                "unknown-retain"
            );
        }
    }

    #[test]
    fn inventory_includes_serialized_fc_runtime_dependencies() {
        let versions = agentenv::snapshot::SnapshotRuntimeVersions::new(
            "historic-kernel".into(),
            "historic-fc".into(),
            "historic-envd".into(),
            "historic-tools".into(),
        );
        let mut record = json!({"version":1, "metadata":{
            "runtime_versions":versions, "virtualization_mode":"pvm",
            "user_metadata":{"kernel_version":"do-not-print"},
            "context":{"env_vars":{"PASSWORD":"do-not-print"}}
        }});
        // Genuine legacy FC omitted the later backend declaration.
        record["metadata"]["runtime_versions"]
            .as_object_mut()
            .unwrap()
            .remove("backend");
        for outer_backend in [Value::Null, json!("firecracker")] {
            record["backend"] = outer_backend;
            let report = summarize(&serde_json::to_vec(&record).unwrap());
            for (field, expected) in [
                ("kernel_version", "historic-kernel"),
                ("firecracker_version", "historic-fc"),
                ("envd_version", "historic-envd"),
                ("tools_drive_version", "historic-tools"),
            ] {
                assert_eq!(
                    report["dependencies"][format!("/metadata/runtime_versions/{field}")],
                    expected
                );
            }
            assert_eq!(
                report["dependencies"]["/metadata/virtualization_mode"],
                "pvm"
            );
            assert!(!report.to_string().contains("do-not-print"));
        }
    }

    #[test]
    fn metadata_backend_and_identity_conflicts_stay_unknown() {
        for record in [
            json!({"version":1,"metadata":{"runtime_versions":{"backend":"future-vmm"}}}),
            json!({"version":1,"backend":"firecracker","metadata":{"runtime_versions":{"backend":"dragonball"}}}),
            json!({"version":1,"metadata":{"runtime_versions":{"backend":null}}}),
            json!({"version":1,"metadata":{"runtime_versions":{"restore_identity":{"backend":"dragonball"}}}}),
            json!({"version":1,"backend":"dragonball","restoreIdentity":{"backend":"dragonball","runner_build_id":"a"},"metadata":{"runtime_versions":{"backend":"dragonball","restore_identity":{"backend":"dragonball","runner_build_id":"b"}}}}),
        ] {
            assert_eq!(
                summarize(&serde_json::to_vec(&record).unwrap())["classification"],
                "unknown-retain",
                "{record}"
            );
        }
    }
}
