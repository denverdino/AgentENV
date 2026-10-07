//! Ordinary-file, probe-only checkpoint validation. Not a capture barrier.
use agentenv_vmm_protocol::ProbeVmConfig;
use anyhow::{Context, Result};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    path::Path,
};

/// Failure before any VMM mutation; the current phase remains usable.
#[derive(Debug)]
pub struct SnapshotPreflightError(anyhow::Error);

impl std::fmt::Display for SnapshotPreflightError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

impl std::error::Error for SnapshotPreflightError {}

pub fn preflight_error(error: anyhow::Error) -> anyhow::Error {
    SnapshotPreflightError(error).into()
}

pub fn validate_snapshot(state: &Path, memory: &Path, machine: &ProbeVmConfig) -> Result<()> {
    let mut bytes = Vec::new();
    File::open(state)?
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= 16 * 1024 * 1024,
        "probe state exceeds 16 MiB"
    );
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    let probe = &value["agentenv_probe"];
    anyhow::ensure!(
        probe["probe_only"] == true
            && probe["identity"] == serde_json::to_value(crate::build_identity())?,
        "incompatible probe identity/profile"
    );
    anyhow::ensure!(
        probe["machine"] == serde_json::to_value(machine)?,
        "probe machine/device layout mismatch"
    );
    anyhow::ensure!(
        value["header"]["format_epoch"] == 1 && value["vm_kvm_state"].is_object(),
        "incomplete VM state/epoch"
    );
    let devices = value["device_states"]
        .as_object()
        .context("missing saved device states")?;
    anyhow::ensure!(
        devices.len() == 3
            && devices.contains_key("block")
            && devices.contains_key("virtio_net")
            && matches!(devices.get("balloon"), Some(balloon)
                if balloon.is_null() || *balloon == serde_json::json!({"devices":[]})),
        "unsupported saved device classes"
    );
    let blocks = devices["block"]["devices"]
        .as_array()
        .context("missing saved block devices")?;
    anyhow::ensure!(
        blocks.len() == machine.block_devices.len(),
        "block device count mismatch"
    );
    for (saved, expected) in blocks.iter().zip(&machine.block_devices) {
        let expected_config = serde_json::json!({
            "device_type": "RawBlock", "drive_id": expected.drive_id,
            "path_on_host": expected.path_on_host, "is_root_device": expected.is_root,
            "is_read_only": expected.read_only, "is_direct": !(expected.is_root && expected.read_only),
            "is_volume_slot":false,"backing_read_only":null,
            "no_drop": false, "num_queues": 1, "part_uuid": null,
            "queue_size": 256, "rate_limiter": null, "sparse": false,
            "use_generic_irq": false, "use_shared_irq": false, "use_pci_bus": false
        });
        anyhow::ensure!(
            saved["config"] == expected_config
                && saved["device_info"]["RawBlock"].is_object()
                && saved["transport"]["Mmio"].is_object(),
            "saved block identity/config/profile mismatch"
        );
    }
    let networks = devices["virtio_net"]["devices"]
        .as_array()
        .context("missing saved network devices")?;
    anyhow::ensure!(networks.len() == 1, "network device count mismatch");
    let network = &networks[0];
    let expected_network = serde_json::json!({
        "backend": {
            "type": "virtio", "iface_id": machine.network.iface_id,
            "host_dev_name": machine.network.tap_name, "allow_duplicate_mac": false,
            "rx_rate_limiter": null, "tx_rate_limiter": null
        },
        "guest_mac": null, "num_queues": null, "queue_size": null,
        "use_generic_irq": false, "use_shared_irq": false
    });
    anyhow::ensure!(
        network["config"] == expected_network
            && network["device_info"].is_object()
            && network["transport"]["Mmio"].is_object(),
        "saved network identity/config/profile mismatch"
    );
    let cpus = value["vcpu_states"]
        .as_array()
        .context("missing vCPU state")?;
    anyhow::ensure!(
        cpus.len() == usize::from(machine.vcpu_count),
        "vCPU count mismatch"
    );
    for (index, cpu) in cpus.iter().enumerate() {
        anyhow::ensure!(
            cpu["id"].as_u64() == Some(index as u64),
            "vCPU identity/order mismatch"
        );
    }
    let regions = value["memory_state"]["regions"]
        .as_array()
        .context("missing memory regions")?;
    anyhow::ensure!(
        regions.len() == 1,
        "M0 requires a single RAM region without firmware"
    );
    let ram = u64::from(machine.memory_mib) * 1024 * 1024;
    anyhow::ensure!(
        ram > 0 && ram < 3 * 1024 * 1024 * 1024,
        "M0 snapshot requires RAM below the MMIO hole"
    );
    let region = &regions[0];
    anyhow::ensure!(
        region["guest_addr"].as_u64() == Some(0)
            && region["size"].as_u64() == Some(ram)
            && region["file_offset"].as_u64() == Some(0),
        "memory region layout mismatch"
    );
    anyhow::ensure!(
        fs::metadata(memory)?.is_file() && fs::metadata(memory)?.len() == ram,
        "memory file size mismatch"
    );
    Ok(())
}

pub fn stamp_identity(state: &Path, machine: &ProbeVmConfig) -> Result<()> {
    let mut value: serde_json::Value = serde_json::from_reader(File::open(state)?)?;
    value["agentenv_probe"] =
        serde_json::json!({"probe_only":true,"identity":crate::build_identity(),"machine":machine});
    serde_json::to_writer(
        OpenOptions::new().write(true).truncate(true).open(state)?,
        &value,
    )?;
    Ok(())
}

pub struct OutputFiles {
    pub state: File,
    pub memory: File,
}
pub fn prepare_outputs(root: &Path, state: &Path, memory: &Path) -> Result<OutputFiles> {
    prepare_outputs_inner(root, state, memory).map_err(preflight_error)
}

pub fn prepare_state_output(root: &Path, state: &Path) -> Result<File> {
    validate_output(&root.canonicalize()?, state)?;
    Ok(OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(state)?)
}

fn validate_output(root: &Path, path: &Path) -> Result<()> {
    anyhow::ensure!(path.is_absolute(), "probe output must be absolute");
    let parent = path.parent().context("output parent")?.canonicalize()?;
    anyhow::ensure!(
        parent.starts_with(root),
        "probe output escapes instance root"
    );
    anyhow::ensure!(path.file_name().is_some(), "output filename missing");
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => anyhow::bail!("probe output already exists"),
    }
}

fn prepare_outputs_inner(root: &Path, state: &Path, memory: &Path) -> Result<OutputFiles> {
    let root = root.canonicalize()?;
    anyhow::ensure!(state != memory, "state and memory outputs must differ");
    for path in [state, memory] {
        validate_output(&root, path)?;
    }
    Ok(OutputFiles {
        state: OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(state)?,
        memory: OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(memory)?,
    })
}
