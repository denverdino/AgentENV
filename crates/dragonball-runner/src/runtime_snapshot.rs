use agentenv_vmm_protocol::runtime::*;
use anyhow::{ensure, Context, Result};
use dragonball::device_manager::blk_dev_mgr::BlockDeviceState;
use dragonball::device_manager::net_dev_mgr::Backend;
use dragonball::device_manager::persist::VirtioTransportState;
use dragonball::snapshot::MicrovmState;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Component, Path};

fn logical_block(id: &str) -> std::path::PathBuf {
    format!("aenv-block:{id}").into()
}
fn logical_net(id: &str) -> String {
    format!("aenv-net:{id}")
}

fn canonical_state(raw: &Value) -> Result<MicrovmState> {
    let state: MicrovmState = serde_json::from_value(raw.clone())?;
    // Exact writer/profile only: reject fields ignored by upstream serde derives.
    ensure!(
        serde_json::to_value(&state)? == *raw,
        "unknown or noncanonical upstream state"
    );
    Ok(state)
}

fn layout(state: &MicrovmState) -> Result<MemoryLayout> {
    let regions = &state
        .memory_state
        .as_ref()
        .context("missing RAM state")?
        .regions;
    let mut image_size = 0u64;
    let regions = regions
        .iter()
        .map(|r| {
            image_size = image_size
                .checked_add(r.size)
                .context("RAM image overflow")?;
            Ok(MemoryRegion {
                guest_base: r.guest_addr,
                image_offset: r.file_offset,
                length: r.size,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let layout = MemoryLayout {
        page_size: 4096,
        regions,
        image_size,
    };
    layout.validate().map_err(anyhow::Error::msg)?;
    Ok(layout)
}

fn block_capacity(info: &dragonball::device_manager::blk_dev_mgr::BlockDevState) -> Result<u64> {
    let BlockDeviceState::RawBlock(info) = &info.device_info;
    let bytes: [u8; 8] = info
        .config_space
        .get(..8)
        .context("missing block capacity")?
        .try_into()?;
    let capacity = u64::from_le_bytes(bytes)
        .checked_mul(512)
        .context("block capacity overflow")?;
    ensure!(
        capacity > 0 && info.acked_features & !info.avail_features == 0,
        "invalid block capacity/features"
    );
    Ok(capacity)
}

fn validate_transport(
    transport: &VirtioTransportState,
    queues: usize,
    max_size: u16,
) -> Result<()> {
    let VirtioTransportState::Mmio(transport) = transport;
    ensure!(
        transport.device_activated && transport.queues.len() == queues,
        "inactive or incomplete MMIO transport"
    );
    for (index, config) in transport.queues.iter().enumerate() {
        let queue = &config.queue;
        ensure!(
            usize::from(config.index) == index
                && queue.max_size == max_size
                && queue.size > 0
                && queue.size <= queue.max_size
                && queue.size.is_power_of_two(),
            "unsupported virtio queue geometry"
        );
    }
    Ok(())
}

fn validate_state(state: &MicrovmState, machine: &RuntimeVmConfig, memory_len: u64) -> Result<()> {
    machine
        .identity
        .validate_profile()
        .map_err(anyhow::Error::msg)?;
    ensure!(
        state.header.format_epoch == UPSTREAM_SNAPSHOT_FORMAT_EPOCH && state.capture_cpu.is_some(),
        "missing held snapshot CPU contract/epoch"
    );
    state.validate_for_restore(machine.vcpu_count)?;
    ensure!(
        state
            .vcpu_states
            .iter()
            .enumerate()
            .all(|(index, cpu)| usize::from(cpu.id) == index),
        "CPU identity/order conflict"
    );
    let memory_layout = layout(state)?;
    ensure!(
        memory_layout.image_size == u64::from(machine.memory_mib) * 1024 * 1024
            && memory_len == memory_layout.image_size,
        "short or mismatched RAM image"
    );
    let blocks = &state
        .device_states
        .block
        .as_ref()
        .context("missing block state")?
        .devices;
    ensure!(
        blocks.len() == machine.block_devices.len(),
        "block set mismatch"
    );
    let mut ids = HashSet::new();
    for saved in blocks {
        ensure!(
            valid_logical_id(&saved.config.drive_id) && ids.insert(&saved.config.drive_id),
            "invalid or duplicate block ID"
        );
        let expected = machine
            .block_devices
            .iter()
            .find(|disk| disk.drive_id == saved.config.drive_id)
            .context("unknown block ID")?;
        let expected = crate::vmm::runtime_block_config(expected);
        ensure!(
            saved.config == expected,
            "block configuration/profile conflict"
        );
        block_capacity(saved)?;
        validate_transport(&saved.transport, 1, 256)?;
    }
    let nets = &state
        .device_states
        .virtio_net
        .as_ref()
        .context("missing network state")?
        .devices;
    ensure!(nets.len() == 1, "network set mismatch");
    ensure!(
        valid_logical_id(&machine.network.iface_id) && ids.insert(&machine.network.iface_id),
        "duplicate network ID"
    );
    ensure!(
        nets[0].config
            == crate::vmm::network_config(&crate::runtime_service::probe_config(machine))
            && nets[0].device_info.acked_features & !nets[0].device_info.avail_features == 0,
        "network configuration/profile conflict"
    );
    validate_transport(&nets[0].transport, 2, 256)?;
    let balloons = &state
        .device_states
        .balloon
        .as_ref()
        .context("missing balloon state")?
        .devices;
    ensure!(
        balloons.len() == 1 && balloons[0].config == crate::vmm::runtime_balloon_config(),
        "balloon configuration/profile conflict"
    );
    validate_transport(&balloons[0].transport, 3, 128)?;
    Ok(())
}

pub fn capture_envelope(
    mut raw: Value,
    machine: &RuntimeVmConfig,
    memory_len: u64,
) -> Result<RuntimeSnapshotEnvelope> {
    crate::runtime_service::validate_runtime_config(machine)?;
    if let Some(stamp) = raw
        .as_object_mut()
        .context("state must be an object")?
        .remove("agentenv_runtime")
    {
        ensure!(
            stamp == serde_json::json!({"identity":machine.identity}),
            "runtime identity stamp conflict"
        );
    }
    let mut state = canonical_state(&raw)?;
    validate_state(&state, machine, memory_len)?;
    let memory_layout = layout(&state)?;
    let mut disks = vec![];
    for saved in &mut state.device_states.block.as_mut().unwrap().devices {
        disks.push(LogicalBlockDevice {
            drive_id: saved.config.drive_id.clone(),
            read_only: saved.config.is_read_only,
            is_root: saved.config.is_root_device,
            is_volume_slot: saved.config.is_volume_slot,
            backing_read_only: saved.config.backing_read_only,
            capacity_bytes: block_capacity(saved)?,
            rate_limiter: machine
                .block_devices
                .iter()
                .find(|disk| disk.drive_id == saved.config.drive_id)
                .context("logical disk missing")?
                .rate_limiter
                .clone(),
        });
        saved.config.path_on_host = logical_block(&saved.config.drive_id);
    }
    let Backend::Virtio(net) = &mut state.device_states.virtio_net.as_mut().unwrap().devices[0]
        .config
        .backend;
    net.host_dev_name = logical_net(&net.iface_id);
    let envelope = RuntimeSnapshotEnvelope {
        envelope_version: 2,
        identity: machine.identity.clone(),
        machine: PortableMachine {
            vcpu_count: machine.vcpu_count,
            memory_mib: machine.memory_mib,
            block_devices: disks,
            network_id: machine.network.iface_id.clone(),
        },
        memory_layout,
        upstream_state: serde_json::to_value(state)?,
    };
    envelope
        .validate_state_topology()
        .map_err(anyhow::Error::msg)?;
    Ok(envelope)
}

fn validate_host_path(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute()
            && !path
                .components()
                .any(|part| matches!(part, Component::ParentDir))
            && !path.as_os_str().as_encoded_bytes().contains(&0),
        "host binding must be absolute and nonescaping"
    );
    Ok(())
}

pub fn restore_state(
    envelope: &RuntimeSnapshotEnvelope,
    machine: &RuntimeVmConfig,
    memory_len: u64,
) -> Result<Value> {
    envelope
        .validate_state_topology()
        .map_err(anyhow::Error::msg)?;
    ensure!(
        envelope
            .identity
            .is_restore_compatible_with(&machine.identity)
            && envelope.machine.vcpu_count == machine.vcpu_count
            && envelope.machine.memory_mib == machine.memory_mib
            && envelope.machine.network_id == machine.network.iface_id
            && envelope.machine.block_devices.len() == machine.block_devices.len(),
        "restore identity/machine conflict"
    );
    crate::runtime_service::validate_runtime_config(machine)?;
    let mut logical = machine.clone();
    validate_host_path(&machine.kernel_path)?;
    for disk in &mut logical.block_devices {
        validate_host_path(&disk.path_on_host)?;
        let saved = envelope
            .machine
            .block_devices
            .iter()
            .find(|saved| saved.drive_id == disk.drive_id)
            .context("logical disk missing")?;
        ensure!(
            disk.read_only == saved.read_only
                && disk.is_root == saved.is_root
                && disk.is_volume_slot == saved.is_volume_slot
                && (saved.is_volume_slot || disk.backing_read_only == saved.backing_read_only),
            "logical disk role conflict"
        );
        disk.path_on_host = logical_block(&disk.drive_id);
        disk.rate_limiter = saved.rate_limiter.clone();
        // Validate opaque captured state against its original backing policy.
        // New access/capacity is applied through the checked transient binding.
        disk.backing_read_only = saved.backing_read_only;
    }
    let mut state = canonical_state(&envelope.upstream_state)?;
    // Only inject the checked TAP into its host config field. The opaque device
    // feature/config-space and transport state remain unchanged.
    let Backend::Virtio(net) = &mut state
        .device_states
        .virtio_net
        .as_mut()
        .context("network missing")?
        .devices
        .get_mut(0)
        .context("network missing")?
        .config
        .backend;
    ensure!(
        net.host_dev_name == logical_net(&machine.network.iface_id),
        "nonlogical network binding"
    );
    net.host_dev_name = machine.network.tap_name.clone();
    validate_state(&state, &logical, memory_len)?;
    ensure!(
        layout(&state)? == envelope.memory_layout,
        "RAM layout envelope conflict"
    );
    for saved in &mut state.device_states.block.as_mut().unwrap().devices {
        let device = envelope
            .machine
            .block_devices
            .iter()
            .find(|disk| disk.drive_id == saved.config.drive_id)
            .context("unknown saved disk")?;
        ensure!(
            block_capacity(saved)? == device.capacity_bytes,
            "block capacity envelope conflict"
        );
        let current = machine
            .block_devices
            .iter()
            .find(|disk| disk.drive_id == device.drive_id)
            .context("host disk missing")?;
        saved.config.path_on_host = current.path_on_host.clone();
        saved.config.rate_limiter = crate::vmm::runtime_block_config(current).rate_limiter;
    }
    Ok(serde_json::to_value(state)?)
}
