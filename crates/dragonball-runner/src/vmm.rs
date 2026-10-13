use std::{
    collections::HashMap,
    fs::OpenOptions,
    os::fd::AsRawFd,
    os::unix::fs::FileTypeExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::service::{validate_config, ProbeVmm};
use agentenv_vmm_protocol::{ProbeBlockDevice, ProbeVmConfig};
use anyhow::Context;
use crossbeam_channel::{unbounded, Receiver, Sender};
use dragonball::{
    api::v1::{
        Backend, BlockDeviceConfigInfo, BootSourceConfig, InstanceInfo, NetworkInterfaceConfig,
        VirtioConfig, VmmAction, VmmData, VmmRequest, VmmResponse, VmmService,
    },
    vm::VmConfigInfo,
    Vmm,
};
use vmm_sys_util::eventfd::EventFd;

pub struct DragonballVmm {
    to_vmm: Sender<VmmRequest>,
    from_vmm: Receiver<VmmResponse>,
    event: EventFd,
    worker: Option<JoinHandle<i32>>,
    deadline: Instant,
    work_root: PathBuf,
    machine: Option<ProbeVmConfig>,
    runtime_machine: Option<agentenv_vmm_protocol::runtime::RuntimeVmConfig>,
    runtime_inputs: Vec<std::fs::File>,
}

fn vm_config(config: &ProbeVmConfig) -> VmConfigInfo {
    VmConfigInfo {
        vcpu_count: config.vcpu_count,
        max_vcpu_count: config.vcpu_count,
        mem_size_mib: config.memory_mib as usize,
        cpu_pm: "off".into(),
        cpu_topology: dragonball::vm::CpuTopology {
            cores_per_die: config.vcpu_count,
            ..Default::default()
        },
        ..Default::default()
    }
}

pub(crate) fn block_config(disk: &ProbeBlockDevice) -> BlockDeviceConfigInfo {
    BlockDeviceConfigInfo {
        drive_id: disk.drive_id.clone(),
        path_on_host: disk.path_on_host.clone(),
        is_read_only: disk.read_only,
        is_root_device: disk.is_root,
        // The shared read-only tools root can reuse the host page cache,
        // matching Firecracker. Keep direct I/O for user disks and volumes.
        is_direct: !(disk.is_root && disk.read_only),
        use_pci_bus: Some(false),
        use_shared_irq: Some(false),
        use_generic_irq: Some(false),
        ..Default::default()
    }
}

pub(crate) fn runtime_block_config(
    disk: &agentenv_vmm_protocol::runtime::RuntimeBlockDevice,
) -> BlockDeviceConfigInfo {
    use dragonball::config_manager::{RateLimiterConfigInfo, TokenBucketConfigInfo};
    let mut config = block_config(&ProbeBlockDevice {
        drive_id: disk.drive_id.clone(),
        path_on_host: disk.path_on_host.clone(),
        read_only: disk.read_only,
        is_root: disk.is_root,
    });
    config.is_volume_slot = disk.is_volume_slot;
    config.backing_read_only = disk.backing_read_only;
    config.rate_limiter = disk.rate_limiter.as_ref().map(|rate| {
        let bucket =
            |value: &agentenv_vmm_protocol::runtime::RuntimeTokenBucket| TokenBucketConfigInfo {
                size: value.size,
                one_time_burst: value.one_time_burst,
                refill_time: value.refill_time,
            };
        RateLimiterConfigInfo {
            bandwidth: bucket(&rate.bandwidth),
            ops: bucket(&rate.ops),
        }
    });
    config
}

pub(crate) fn network_config(config: &ProbeVmConfig) -> NetworkInterfaceConfig {
    NetworkInterfaceConfig {
        use_shared_irq: Some(false),
        use_generic_irq: Some(false),
        backend: Backend::Virtio(VirtioConfig {
            iface_id: config.network.iface_id.clone(),
            host_dev_name: config.network.tap_name.clone(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

pub(crate) fn runtime_balloon_config(
) -> dragonball::device_manager::balloon_dev_mgr::BalloonDeviceConfigInfo {
    dragonball::device_manager::balloon_dev_mgr::BalloonDeviceConfigInfo {
        balloon_id: "memory".into(),
        size_mib: 0,
        use_shared_irq: Some(false),
        use_generic_irq: Some(false),
        f_deflate_on_oom: true,
        f_reporting: true,
    }
}

impl DragonballVmm {
    /// The CLI installs seccomp before calling this constructor; it is not
    /// safe to use from the multithreaded AgentENV node process.
    pub fn new(instance_id: &str, work_root: &Path) -> anyhow::Result<Self> {
        let event = EventFd::new(libc::EFD_NONBLOCK)?;
        let shared = Arc::new(RwLock::new(InstanceInfo::new(
            instance_id.into(),
            env!("CARGO_PKG_VERSION").into(),
            None,
        )));
        // None lets KvmContext own and close its /dev/kvm descriptor even when
        // initialization fails. No externally leaked into_raw_fd ownership.
        let vmm = Vmm::new(shared, event.try_clone()?, HashMap::new(), None)
            .context("initialize Dragonball KVM (/dev/kvm required)")?;
        let (to_vmm, from_api) = unbounded();
        let (to_api, from_vmm) = unbounded();
        let service = VmmService::new(from_api, to_api);
        let worker = thread::Builder::new()
            .name("dragonball-vmm".into())
            .spawn(move || Vmm::run_vmm_event_loop(Arc::new(Mutex::new(vmm)), service))?;
        let mut adapter = Self {
            to_vmm,
            from_vmm,
            event,
            worker: Some(worker),
            deadline: Instant::now() + Duration::from_secs(5),
            work_root: work_root.canonicalize()?,
            machine: None,
            runtime_machine: None,
            runtime_inputs: Vec::new(),
        };
        // Readiness includes the initialized API event loop, not just a spawned
        // thread. On failure Drop attempts shutdown within a bounded deadline.
        adapter.action(VmmAction::GetVmConfiguration)?;
        Ok(adapter)
    }

    fn remaining(&self) -> anyhow::Result<Duration> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(!remaining.is_zero(), "VMM request deadline exceeded");
        Ok(remaining)
    }

    fn action(&mut self, action: VmmAction) -> anyhow::Result<VmmData> {
        self.remaining()?;
        anyhow::ensure!(
            self.worker.as_ref().is_some_and(|w| !w.is_finished()),
            "VMM worker is not running"
        );
        self.to_vmm
            .send(Box::new(action))
            .context("send Dragonball action")?;
        self.event.write(1).context("notify Dragonball action")?;
        (*self
            .from_vmm
            .recv_timeout(self.remaining()?)
            .context("wait for Dragonball action")?)
        .map_err(anyhow::Error::from)
    }

    fn join_worker(&mut self) -> anyhow::Result<()> {
        while self.worker.as_ref().is_some_and(|w| !w.is_finished()) {
            let remaining = self.remaining()?;
            thread::sleep(remaining.min(Duration::from_millis(1)));
        }
        if let Some(worker) = self.worker.take() {
            let code = worker
                .join()
                .map_err(|_| anyhow::anyhow!("VMM worker panicked"))?;
            anyhow::ensure!(code == 0, "VMM exited with code {code}");
        }
        Ok(())
    }
}

impl ProbeVmm for DragonballVmm {
    fn set_budget(&mut self, budget: Duration) {
        self.deadline = Instant::now() + budget.min(Duration::from_secs(30));
    }

    fn configure(&mut self, config: &ProbeVmConfig) -> anyhow::Result<()> {
        validate_config(config)?;
        // Validate all files before configuring any devices/starting workers.
        anyhow::ensure!(config.kernel_path.is_file(), "kernel file does not exist");
        for disk in &config.block_devices {
            let file = OpenOptions::new()
                .read(true)
                .write(
                    !self
                        .runtime_machine
                        .as_ref()
                        .and_then(|m| m.block_devices.iter().find(|d| d.drive_id == disk.drive_id))
                        .and_then(|d| d.backing_read_only)
                        .unwrap_or(disk.read_only),
                )
                .open(&disk.path_on_host)
                .context("validate probe block input")?;
            let kind = file.metadata()?.file_type();
            anyhow::ensure!(
                kind.is_file() || kind.is_block_device(),
                "probe disk must be a file or block device"
            );
        }
        self.action(VmmAction::SetVmConfiguration(vm_config(config)))?;
        self.action(VmmAction::ConfigureBootSource(BootSourceConfig {
            kernel_path: config
                .kernel_path
                .to_str()
                .context("kernel path must be UTF-8")?
                .into(),
            boot_args: Some(config.boot_args.clone()),
            initrd_path: None,
            firmware_path: None,
        }))?;
        for disk in &config.block_devices {
            let mut device = block_config(disk);
            if let Some(runtime) = &self.runtime_machine {
                let policy = runtime
                    .block_devices
                    .iter()
                    .find(|current| current.drive_id == disk.drive_id)
                    .context("runtime disk policy missing")?;
                device.rate_limiter = runtime_block_config(policy).rate_limiter;
                device.is_volume_slot = policy.is_volume_slot;
                device.backing_read_only = policy.backing_read_only;
            }
            self.action(VmmAction::InsertBlockDevice(device))?;
        }
        self.action(VmmAction::InsertNetworkDevice(network_config(config)))?;
        self.machine = Some(config.clone());
        Ok(())
    }
    fn start(&mut self) -> anyhow::Result<()> {
        self.action(VmmAction::StartMicroVm)?;
        Ok(())
    }
    fn save_quiesced(&mut self, state: &Path, memory: &Path) -> anyhow::Result<()> {
        let files = crate::probe_snapshot::prepare_outputs(&self.work_root, state, memory)?;
        // The upstream API opens/truncates by path. Refer to the already-owned
        // descriptors so replacing a named output with a symlink cannot redirect it.
        let state_fd = format!("/proc/self/fd/{}", files.state.as_raw_fd());
        let memory_fd = format!("/proc/self/fd/{}", files.memory.as_raw_fd());
        self.action(VmmAction::SaveMicrovm {
            state_path: state_fd.clone(),
            mem_path: memory_fd,
        })?;
        let machine = self.machine.as_ref().context("probe machine missing")?;
        crate::probe_snapshot::stamp_identity(Path::new(&state_fd), machine)?;
        files.state.sync_all()?;
        files.memory.sync_all()?;
        Ok(())
    }
    fn restore_quiesced(&mut self, state: &Path, memory: &Path) -> anyhow::Result<()> {
        let machine = self.machine.as_ref().context("probe machine missing")?;
        // Keep the exact validated inputs open across the upstream reopen/mmap.
        let state_file = std::fs::File::open(state)
            .map_err(|error| crate::probe_snapshot::preflight_error(error.into()))?;
        let memory_file = std::fs::File::open(memory)
            .map_err(|error| crate::probe_snapshot::preflight_error(error.into()))?;
        let state_fd = format!("/proc/self/fd/{}", state_file.as_raw_fd());
        let memory_fd = format!("/proc/self/fd/{}", memory_file.as_raw_fd());
        crate::probe_snapshot::validate_snapshot(
            Path::new(&state_fd),
            Path::new(&memory_fd),
            machine,
        )
        .map_err(crate::probe_snapshot::preflight_error)?;
        // Typed upstream validation is still required; the outer probe header
        // does not replace full KVM/device deserialization or RAM-layout checks.
        dragonball::snapshot::MicrovmState::load_from_file(Path::new(&state_fd))
            .map_err(|error| crate::probe_snapshot::preflight_error(error.into()))?
            .validate_for_restore(machine.vcpu_count)
            .map_err(|error| crate::probe_snapshot::preflight_error(error.into()))?;
        self.action(VmmAction::StartMicroVmFromSnapshot {
            state_path: state_fd,
            mem_path: memory_fd,
        })?;
        Ok(())
    }
    fn pause(&mut self) -> anyhow::Result<()> {
        self.action(VmmAction::PauseMicroVm)?;
        Ok(())
    }
    fn resume(&mut self) -> anyhow::Result<()> {
        self.action(VmmAction::ResumeMicroVm)?;
        Ok(())
    }
    fn stop(&mut self) -> anyhow::Result<()> {
        if self.worker.as_ref().is_some_and(|w| !w.is_finished()) {
            self.action(VmmAction::ShutdownMicroVm)?;
        }
        self.join_worker()
    }
}

impl Drop for DragonballVmm {
    fn drop(&mut self) {
        ProbeVmm::set_budget(self, Duration::from_secs(1));
        // A failed/blocked stop is not claimed clean: the CLI exits the entire
        // process; the node must kill+wait before releasing any host resources.
        let _ = ProbeVmm::stop(self);
    }
}

impl DragonballVmm {
    fn runtime_action(&mut self, action: VmmAction) -> anyhow::Result<VmmData> {
        self.action(action).map_err(|error| {
            use dragonball::{api::v1::VmmActionError, snapshot::capture::CaptureError};
            if matches!(
                error.downcast_ref::<VmmActionError>(),
                Some(VmmActionError::Capture(
                    CaptureError::Rejected(_) | CaptureError::Export(_)
                ))
            ) {
                crate::runtime_service::RuntimeKnownError(error).into()
            } else {
                error
            }
        })
    }

    fn runtime_bindings(&self) -> anyhow::Result<agentenv_vmm_protocol::runtime::RuntimeVmConfig> {
        let mut bindings = self
            .runtime_machine
            .as_ref()
            .context("runtime machine missing")?
            .clone();
        let opened = self.machine.as_ref().context("opened machine missing")?;
        bindings.kernel_path = opened.kernel_path.clone();
        for disk in &mut bindings.block_devices {
            disk.path_on_host = opened
                .block_devices
                .iter()
                .find(|current| current.drive_id == disk.drive_id)
                .context("opened logical disk missing")?
                .path_on_host
                .clone();
        }
        bindings.network.tap_name = opened.network.tap_name.clone();
        Ok(bindings)
    }

    fn runtime_report(
        data: VmmData,
    ) -> anyhow::Result<agentenv_vmm_protocol::runtime::CaptureReport> {
        use agentenv_vmm_protocol::runtime::{
            CaptureReport, CaptureToken, MemoryLayout, MemoryRegion, WorkerAck,
        };
        let VmmData::CaptureReport(report) = data else {
            anyhow::bail!("Dragonball did not return a capture proof");
        };
        anyhow::ensure!(
            report
                .vcpu_acks
                .iter()
                .all(|ack| ack.generation == report.generation),
            "stale CPU acknowledgement generation"
        );
        Ok(CaptureReport {
            token: CaptureToken {
                runner_instance_id: String::new(),
                generation: report.generation,
            },
            memory_layout: MemoryLayout {
                page_size: 4096,
                image_size: report.memory_layout.total_bytes,
                regions: report
                    .memory_layout
                    .regions
                    .into_iter()
                    .map(|region| MemoryRegion {
                        guest_base: region.guest_addr,
                        image_offset: region.file_offset,
                        length: region.size,
                    })
                    .collect(),
            },
            vcpu_ids: report
                .vcpu_acks
                .into_iter()
                .map(|ack| ack.vcpu_id)
                .collect(),
            worker_acks: report
                .device_acks
                .into_iter()
                .map(|ack| WorkerAck {
                    device_id: ack.device_id,
                    generation: ack.generation,
                    pending_io: ack.pending_io,
                    memory_writers: ack.memory_writers,
                    flush_completed: ack.flush_completed,
                })
                .collect(),
        })
    }

    fn stamp_runtime_state(&self, state: &std::fs::File, memory_len: u64) -> anyhow::Result<()> {
        use std::io::{Seek, Write};
        let stamp = (|| -> anyhow::Result<()> {
            let mut reader = state.try_clone()?;
            reader.seek(std::io::SeekFrom::Start(0))?;
            let value: serde_json::Value = serde_json::from_reader(reader)?;
            let envelope = crate::runtime_snapshot::capture_envelope(
                value,
                &self.runtime_bindings()?,
                memory_len,
            )?;
            let mut writer = state.try_clone()?;
            writer.seek(std::io::SeekFrom::Start(0))?;
            writer.set_len(0)?;
            serde_json::to_writer(&mut writer, &envelope)?;
            writer.flush()?;
            writer.sync_all()?;
            Ok(())
        })();
        stamp.map_err(|error| anyhow::Error::new(crate::runtime_service::RuntimeKnownError(error)))
    }
}

impl crate::runtime_service::RuntimeVmm for DragonballVmm {
    fn set_budget(&mut self, budget: Duration) {
        self.deadline = Instant::now() + budget.min(Duration::from_secs(60));
    }

    fn configure(
        &mut self,
        config: &agentenv_vmm_protocol::runtime::RuntimeVmConfig,
    ) -> anyhow::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        crate::runtime_service::validate_runtime_config(config)?;
        // Retain exact input inodes across delayed boot and device activation.
        let mut machine = crate::runtime_service::probe_config(config);
        let prepared = (|| -> anyhow::Result<Vec<std::fs::File>> {
            let kernel = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&config.kernel_path)?;
            anyhow::ensure!(
                kernel.metadata()?.is_file(),
                "kernel must be a regular file"
            );
            machine.kernel_path = format!("/proc/self/fd/{}", kernel.as_raw_fd()).into();
            let mut files = vec![kernel];
            for disk in &mut machine.block_devices {
                let policy = config
                    .block_devices
                    .iter()
                    .find(|d| d.drive_id == disk.drive_id)
                    .context("disk policy missing")?;
                let file = OpenOptions::new()
                    .read(true)
                    .write(!policy.backing_read_only.unwrap_or(disk.read_only))
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&disk.path_on_host)?;
                let kind = file.metadata()?.file_type();
                anyhow::ensure!(
                    kind.is_file() || kind.is_block_device(),
                    "runtime disk must be a file or block device"
                );
                disk.path_on_host = format!("/proc/self/fd/{}", file.as_raw_fd()).into();
                files.push(file);
            }
            Ok(files)
        })()
        .map_err(|error| anyhow::Error::new(crate::runtime_service::RuntimeKnownError(error)))?;
        self.runtime_inputs = prepared;
        self.runtime_machine = Some(config.clone());
        ProbeVmm::configure(self, &machine)?;
        self.action(VmmAction::InsertBalloonDevice(runtime_balloon_config()))?;
        Ok(())
    }

    fn start(&mut self) -> anyhow::Result<()> {
        ProbeVmm::start(self)
    }

    fn begin_capture(
        &mut self,
        generation: u64,
    ) -> anyhow::Result<agentenv_vmm_protocol::runtime::CaptureReport> {
        let data = self.runtime_action(VmmAction::BeginCapture {
            generation,
            deadline: self.deadline,
        })?;
        Self::runtime_report(data)
    }

    fn export_held(&mut self, generation: u64, state: &Path, memory: &Path) -> anyhow::Result<()> {
        let outputs = crate::probe_snapshot::prepare_outputs(&self.work_root, state, memory)
            .map_err(|error| {
                anyhow::Error::new(crate::runtime_service::RuntimeKnownError(error))
            })?;
        let files =
            dragonball::snapshot::capture::SnapshotFiles::new(outputs.state, outputs.memory);
        self.runtime_action(VmmAction::ExportHeldSnapshot {
            generation,
            deadline: self.deadline,
            files: files.clone(),
        })?;
        // Persist only portable provenance, never transient capture tokens or host addresses.
        self.stamp_runtime_state(&files.state, files.memory.metadata()?.len())
    }

    fn export_held_state(&mut self, generation: u64, state: &Path) -> anyhow::Result<()> {
        let state = crate::probe_snapshot::prepare_state_output(&self.work_root, state).map_err(
            |error| anyhow::Error::new(crate::runtime_service::RuntimeKnownError(error)),
        )?;
        let state = dragonball::snapshot::capture::SnapshotStateFile(Arc::new(state));
        self.runtime_action(VmmAction::ExportHeldState {
            generation,
            deadline: self.deadline,
            state_file: state.clone(),
        })?;
        let memory_len = u64::from(self.runtime_bindings()?.memory_mib) * 1024 * 1024;
        self.stamp_runtime_state(&state.0, memory_len)
    }

    fn export_memory_map(
        &mut self,
        generation: u64,
    ) -> anyhow::Result<agentenv_vmm_protocol::runtime_memory::FrozenMemoryMap> {
        use agentenv_vmm_protocol::{runtime::CaptureToken, runtime_memory::*};
        // Captures classify zeros while copying RAM; incremental captures still
        // include every changed range, including pages rewritten to zero.
        let data = self.runtime_action(VmmAction::ExportHeldMemoryMapWithoutZeroScan {
            generation,
            deadline: self.deadline,
        })?;
        let VmmData::FrozenMemoryMap(map) = data else {
            anyhow::bail!("Dragonball did not return a held memory map");
        };
        anyhow::ensure!(
            map.generation == generation,
            "memory map generation mismatch"
        );
        Ok(FrozenMemoryMap {
            token: CaptureToken {
                runner_instance_id: String::new(),
                generation: map.generation,
            },
            layout_id: "packed-ram-4k-v2".into(),
            page_size: map.page_size,
            regions: map
                .regions
                .into_iter()
                .map(|r| FrozenMemoryRegion {
                    guest_base: r.guest_base,
                    host_base: r.host_base,
                    image_offset: r.image_offset,
                    length: r.length,
                })
                .collect(),
            changed_ranges: map
                .changed_ranges
                .into_iter()
                .map(|r| MemoryRange {
                    image_offset: r.image_offset,
                    length: r.length,
                })
                .collect(),
            zero_ranges: map
                .zero_ranges
                .into_iter()
                .map(|r| MemoryRange {
                    image_offset: r.image_offset,
                    length: r.length,
                })
                .collect(),
        })
    }

    fn release_memory_map(&mut self, generation: u64) -> anyhow::Result<()> {
        self.runtime_action(VmmAction::ReleaseHeldMemoryMap { generation })?;
        Ok(())
    }

    fn end_capture(&mut self, generation: u64) -> anyhow::Result<()> {
        self.runtime_action(VmmAction::EndCapture {
            generation,
            deadline: self.deadline,
            resume: true,
        })?;
        Ok(())
    }

    fn load_held(
        &mut self,
        generation: u64,
        input: &agentenv_vmm_protocol::runtime::RuntimeSnapshotInput,
    ) -> anyhow::Result<agentenv_vmm_protocol::runtime::CaptureReport> {
        use std::{
            io::{BufWriter, Read, Seek, Write},
            os::unix::fs::OpenOptionsExt,
        };
        let started = Instant::now();
        let (mut decoded_us, mut rebound_us, mut checked_us) = (0u64, 0u64, 0u64);
        let files = (|| -> anyhow::Result<dragonball::snapshot::capture::SnapshotFiles> {
            anyhow::ensure!(
                input.state_path.is_absolute() && input.memory_path.is_absolute(),
                "snapshot inputs must be absolute"
            );
            let state = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&input.state_path)?;
            let memory = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&input.memory_path)?;
            anyhow::ensure!(
                state.metadata()?.is_file(),
                "snapshot state must be a regular file"
            );
            let backing = dragonball::address_space_manager::inspect_memory_backing(&memory)?;
            let mut bytes = Vec::new();
            state
                .try_clone()?
                .take(16 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            anyhow::ensure!(
                bytes.len() <= 16 * 1024 * 1024,
                "snapshot state exceeds 16 MiB"
            );
            let value: serde_json::Value = serde_json::from_slice(&bytes)?;
            anyhow::ensure!(
                value.get("agentenv_probe").is_none() && !input.probe_only,
                "probe snapshots are not production input"
            );
            let envelope: agentenv_vmm_protocol::runtime::RuntimeSnapshotEnvelope =
                serde_json::from_value(value)?;
            anyhow::ensure!(
                envelope.identity == input.identity,
                "snapshot runtime identity mismatch"
            );
            decoded_us = started.elapsed().as_micros() as u64;
            let bindings = self.runtime_bindings()?;
            let restored =
                crate::runtime_snapshot::restore_state(&envelope, &bindings, backing.capacity)?;
            rebound_us = started.elapsed().as_micros() as u64;
            let mut block_bindings = Vec::new();
            for disk in &envelope.machine.block_devices {
                let current = bindings
                    .block_devices
                    .iter()
                    .find(|current| current.drive_id == disk.drive_id)
                    .context("opened disk missing")?;
                let path = &current.path_on_host;
                let mut file = self
                    .runtime_inputs
                    .iter()
                    .find(|file| *path == Path::new(&format!("/proc/self/fd/{}", file.as_raw_fd())))
                    .context("disk binding does not reference a retained input FD")?;
                let capacity =
                    dragonball::address_space_manager::inspect_memory_backing(file)?.capacity;
                if disk.is_volume_slot && !disk.read_only {
                    block_bindings.push(
                        dragonball::device_manager::blk_dev_mgr::RestoreBlockBinding {
                            slot_id: disk.drive_id.clone(),
                            path: path.clone(),
                            capacity_bytes: capacity,
                            backing_read_only: current
                                .backing_read_only
                                .unwrap_or(current.read_only),
                        },
                    );
                } else {
                    anyhow::ensure!(
                        capacity == disk.capacity_bytes,
                        "disk backing capacity conflict"
                    );
                }
                file.seek(std::io::SeekFrom::Start(0))?;
            }
            checked_us = started.elapsed().as_micros() as u64;
            // Rehydrate only into a private, immediately unlinked owned file.
            let local_path = self.work_root.join(format!(".restore-{generation}.json"));
            let mut local_state = OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&local_path)?;
            std::fs::remove_file(&local_path)?;
            {
                let mut writer = BufWriter::new(&mut local_state);
                serde_json::to_writer(&mut writer, &restored)?;
                writer.flush()?;
            }
            local_state.seek(std::io::SeekFrom::Start(0))?;
            let mut files = dragonball::snapshot::capture::SnapshotFiles::new(local_state, memory);
            files.block_bindings = block_bindings;
            Ok(files)
        })()
        .map_err(|error| anyhow::Error::new(crate::runtime_service::RuntimeKnownError(error)))?;
        let serialized_us = started.elapsed().as_micros() as u64;
        let data = self.runtime_action(VmmAction::LoadSnapshotHeld {
            generation,
            deadline: self.deadline,
            files,
        })?;
        tracing::debug!(
            generation,
            decoded_us,
            rebound_us,
            checked_us,
            serialized_us,
            completed_us = started.elapsed().as_micros() as u64,
            "Dragonball LoadHeld adapter timing (cumulative)"
        );
        Self::runtime_report(data)
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        ProbeVmm::stop(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentenv_vmm_protocol::ProbeNetworkDevice;

    #[test]
    fn m2_runtime_adapter_exports_and_releases_owned_map() {
        use crate::runtime_service::RuntimeVmm;
        use dragonball::memory_tracking::{FrozenMemoryMap, FrozenMemoryRegion, MemoryRange};
        let work = tempfile::tempdir().unwrap();
        let (to_vmm, requests) = unbounded::<VmmRequest>();
        let (responses, from_vmm) = unbounded();
        let map = FrozenMemoryMap {
            generation: 7,
            page_size: 4096,
            regions: vec![FrozenMemoryRegion {
                guest_base: 0,
                host_base: 0x100000,
                image_offset: 0,
                length: 4096,
            }],
            changed_ranges: vec![MemoryRange {
                image_offset: 0,
                length: 4096,
            }],
            zero_ranges: vec![],
        };
        let worker = thread::spawn(move || {
            while let Ok(request) = requests.recv() {
                let (reply, stop) = match *request {
                    VmmAction::ExportHeldMemoryMapWithoutZeroScan { generation: 7, .. } => {
                        (VmmData::FrozenMemoryMap(map.clone()), false)
                    }
                    VmmAction::ReleaseHeldMemoryMap { generation: 7 } => (VmmData::Empty, false),
                    VmmAction::ShutdownMicroVm => (VmmData::Empty, true),
                    other => panic!("unexpected action: {other:?}"),
                };
                responses.send(Box::new(Ok(reply))).unwrap();
                if stop {
                    break;
                }
            }
            0
        });
        let mut adapter = DragonballVmm {
            to_vmm,
            from_vmm,
            event: EventFd::new(libc::EFD_NONBLOCK).unwrap(),
            worker: Some(worker),
            deadline: Instant::now() + Duration::from_secs(5),
            work_root: work.path().to_owned(),
            machine: None,
            runtime_machine: None,
            runtime_inputs: vec![],
        };
        let map = RuntimeVmm::export_memory_map(&mut adapter, 7).unwrap();
        assert_eq!(map.token.generation, 7);
        assert_eq!(map.regions[0].host_base, 0x100000);
        assert_eq!(map.changed_ranges[0].length, 4096);
        RuntimeVmm::release_memory_map(&mut adapter, 7).unwrap();
        RuntimeVmm::stop(&mut adapter).unwrap();
    }

    #[test]
    #[ignore = "requires the real Dragonball KVM adapter"]
    fn runtime_adapter_preserves_full_memory_export_budget() {
        use crate::runtime_service::RuntimeVmm;
        let work = tempfile::tempdir().unwrap();
        let mut adapter = DragonballVmm::new("runtime-budget", work.path()).unwrap();
        RuntimeVmm::set_budget(&mut adapter, Duration::from_secs(60));
        assert!(
            adapter.remaining().unwrap() > Duration::from_secs(59),
            "runtime full-memory export was silently clipped to the M0 probe budget"
        );
        RuntimeVmm::set_budget(&mut adapter, Duration::from_secs(5));
        assert!(adapter.remaining().unwrap() <= Duration::from_secs(5));
        RuntimeVmm::set_budget(&mut adapter, Duration::from_secs(u64::MAX));
        assert!(adapter.remaining().unwrap() <= Duration::from_secs(60));
        ProbeVmm::set_budget(&mut adapter, Duration::from_secs(60));
        assert!(
            adapter.remaining().unwrap() <= Duration::from_secs(30),
            "legacy M0 budget must remain bounded by its original limit"
        );
    }

    #[test]
    fn runtime_adapter_rejects_wrong_cpu_ack_generation() {
        use dragonball::snapshot::capture::{CaptureReport, MemoryLayout, VcpuAck};
        let report = CaptureReport {
            generation: 7,
            vcpu_acks: vec![VcpuAck {
                vcpu_id: 0,
                generation: 6,
            }],
            device_acks: Vec::new(),
            memory_layout: MemoryLayout {
                regions: Vec::new(),
                total_bytes: 0,
            },
        };
        assert!(
            DragonballVmm::runtime_report(VmmData::CaptureReport(report)).is_err(),
            "adapter must validate generation before dropping CPU ack metadata"
        );
    }

    #[test]
    fn tools_reads_are_buffered_without_changing_writable_or_data_drives() {
        for (is_root, read_only, direct) in [
            (true, true, false),
            (true, false, true),
            (false, true, true),
            (false, false, true),
        ] {
            let disk = ProbeBlockDevice {
                drive_id: "disk".into(),
                path_on_host: "/disk.ext4".into(),
                read_only,
                is_root,
            };
            let block = block_config(&disk);
            assert_eq!(block.is_direct, direct, "root={is_root}, ro={read_only}");
            assert_eq!(block.is_read_only, read_only);
        }
    }

    #[test]
    fn m0_uses_standard_mmio_interrupts_and_explicit_cpu_topology() {
        let disk = ProbeBlockDevice {
            drive_id: "rootfs".into(),
            path_on_host: "/tools.ext4".into(),
            read_only: true,
            is_root: true,
        };
        let config = ProbeVmConfig {
            vcpu_count: 2,
            memory_mib: 512,
            kernel_path: "/kernel".into(),
            boot_args: String::new(),
            block_devices: vec![disk.clone()],
            network: ProbeNetworkDevice {
                iface_id: "eth0".into(),
                tap_name: "tap0".into(),
            },
        };
        let machine = vm_config(&config);
        assert_eq!(
            machine.cpu_pm, "off",
            "M0 disables unqualified CPU power-management policy"
        );
        assert_eq!(machine.cpu_topology.cores_per_die, 2);
        let block = block_config(&disk);
        assert_eq!(block.use_shared_irq, Some(false));
        assert_eq!(block.use_generic_irq, Some(false));
        let network = network_config(&config);
        assert_eq!(network.use_shared_irq, Some(false));
        assert_eq!(network.use_generic_irq, Some(false));
    }
}
