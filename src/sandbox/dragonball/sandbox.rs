use crate::snapshot::SnapshotVolumeSlot;
use std::{
    collections::HashSet,
    fs::{self, File},
    net::Ipv4Addr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use agentenv_vmm_protocol::runtime::*;
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use uvm_ublk_daemon::CreateOverlaybdRuntimeDeviceRequest;

use super::{
    capture::DragonballPausedState, factory::DragonballSandboxFactory,
    process::spawn_logged_runner, resources::RuntimeResources, runtime_client::RuntimeClient,
};
use crate::sandbox::{
    custom_extension::{
        custom_extension_params_is_empty, CustomExtensionHookGuard, CustomExtensionParams,
    },
    dragonball::restore_identity::RestoreReader,
    envd::EnvdInstance,
    extra_drive::{
        build_drives_boot_arg, prepare_extra_drives, prepare_volume_drive_slots,
        ExtraDrivePrepareMode, VOLUME_DRIVE_SLOT_PREFIX,
    },
    network::metadata::MetadataListener,
    ublk::OverlaybdRuntimeHandle,
    CapturedSandboxSnapshot, Executor, FreshSandboxBuildSpec, NetworkManager, PausedSandboxState,
    RuntimeArtifactSet, SandboxBackend, SandboxCaptureError, SandboxCaptureResult, SandboxExecutor,
    SandboxForkResult, SandboxForkSpec, SandboxLaunchConfig, SandboxMetadataDocument,
    SandboxNetworkPolicy, SandboxRuntimeInfo, UblkDeviceManager,
};

pub struct DragonballSandbox {
    pub(super) factory: DragonballSandboxFactory,
    pub(super) spec: FreshSandboxBuildSpec,
    pub(super) launch: SandboxLaunchConfig,
    pub(super) runtime: Option<RuntimeStarted>,
    pub(super) restore: Option<Arc<DragonballPausedState>>,
    pub(super) recording: Option<super::startup_pack::RecordingConfig>,
    pub(super) require_guest_identity: bool,
}

pub(super) struct RuntimeStarted {
    // Drop uses the previously qualified reap-before-leases guard, also on cancellation.
    pub(super) resources: RuntimeResources,
    pub(super) client: Arc<tokio::sync::Mutex<RuntimeClient>>,
    pub(super) envd: EnvdInstance,
    pub(super) pid: u32,
    pub(super) machine: RuntimeVmConfig,
    pub(super) memory_base: Option<Arc<super::memory::DragonballMemoryBase>>,
    startup_prefetch: Option<crate::sandbox::startup_pack::LocalStartupPrefetch>,
    pub(super) root: Option<OverlaybdRuntimeHandle>,
    pub(super) extras: Vec<OverlaybdRuntimeHandle>,
    pub(super) volume_slots: Vec<SnapshotVolumeSlot>,
    new_volume_mounts: Vec<(usize, crate::sandbox::ExtraDrive)>,
    pub(super) ready: AtomicBool,
    readiness_failed: AtomicBool,
    extension: Option<CustomExtensionHookGuard>,
    pub(super) frozen_volume_mounts: Vec<(PathBuf, String)>,
}

impl Drop for RuntimeStarted {
    fn drop(&mut self) {
        self.envd.invalidate();
    }
}

impl DragonballSandbox {
    /// Private, transient runtime directory, not a committed artifact location.
    pub fn work_dir(&self) -> Result<&Path> {
        Ok(self
            .runtime
            .as_ref()
            .context("Dragonball sandbox not started")?
            .resources
            .work
            .path())
    }

    pub async fn runtime_status(&mut self) -> Result<RuntimeStatus> {
        let runtime = self
            .runtime
            .as_ref()
            .context("Dragonball sandbox not started")?;
        let RuntimeReply::Status(status) = runtime
            .client
            .lock()
            .await
            .request(RuntimeCommand::Status, Duration::from_secs(5))
            .await?
        else {
            anyhow::bail!("unexpected runtime Status");
        };
        Ok(status)
    }
    pub(super) fn new(
        factory: DragonballSandboxFactory,
        mut spec: FreshSandboxBuildSpec,
        mut launch: SandboxLaunchConfig,
    ) -> Result<Self> {
        factory.validate_cpu_policy()?;
        ensure!(
            spec.resources.cpu_count > 0
                && spec.resources.cpu_count <= u32::from(u8::MAX)
                && spec.resources.memory_mib > 0,
            "unsupported CPU/memory geometry"
        );
        ensure!(
            spec.image_config_path.is_absolute() && spec.image_config_path.is_file(),
            "missing absolute rootfs image config"
        );
        ensure!(
            spec.extra_boot_args.as_deref().is_none_or(str::is_empty),
            "Dragonball extra boot argument policy is not yet qualified"
        );
        spec.extra_drives.extend(launch.extra_drives.clone());
        launch.extra_drives.clear();
        ensure!(
            spec.extra_drives.len() <= 24,
            "too many physical extra drives"
        );
        let mut ids = HashSet::new();
        for drive in &spec.extra_drives {
            crate::sandbox::validate_drive_id(drive.drive_id())?;
            ensure!(ids.insert(drive.drive_id()), "duplicate extra drive ID");
        }
        if factory.policy.extension.is_none() {
            ensure!(
                custom_extension_params_is_empty(launch.custom_extension_params.as_ref()),
                "nonempty custom extension params require hooks"
            );
        }
        Ok(Self {
            factory,
            spec,
            launch,
            runtime: None,
            restore: None,
            recording: None,
            require_guest_identity: true,
        })
    }

    async fn boot(
        factory: DragonballSandboxFactory,
        spec: FreshSandboxBuildSpec,
        launch: SandboxLaunchConfig,
        restore: Option<Arc<DragonballPausedState>>,
        recording: Option<super::startup_pack::RecordingConfig>,
    ) -> Result<RuntimeStarted> {
        let started = std::time::Instant::now();
        let attached_count = spec
            .extra_drives
            .iter()
            .filter(|drive| !drive.is_volume())
            .count();
        let saved_slots = if let Some(saved) = &restore {
            saved.artifacts.manifest.volume_slots.clone()
        } else {
            (0..factory
                .policy
                .app
                .volume
                .max_volume_count
                .min(24 - attached_count))
                .map(|index| {
                    let drive = spec.extra_drives.get(attached_count + index);
                    SnapshotVolumeSlot {
                        slot_id: format!("{VOLUME_DRIVE_SLOT_PREFIX}{index}"),
                        mount_path: drive.map(|d| d.mount_path().into()),
                        sub_path: drive.and_then(|d| d.sub_path().map(Into::into)),
                        guest_read_only: drive.is_some_and(|d| d.read_only()),
                        capacity_bytes: drive.and_then(|d| d.virtual_size()).unwrap_or(4096),
                    }
                })
                .collect()
        };
        ensure!(
            attached_count + saved_slots.len() <= 24,
            "reserved slot budget exceeded"
        );
        let volume_slots = if saved_slots.is_empty() {
            vec![]
        } else {
            let (slots, ordered) = super::slots::bind(
                &saved_slots,
                spec.extra_drives
                    .iter()
                    .filter(|d| d.is_volume())
                    .cloned()
                    .collect(),
            )?;
            ensure!(
                ordered
                    .iter()
                    .map(|d| d.drive_id())
                    .eq(spec.extra_drives[attached_count..]
                        .iter()
                        .map(|d| d.drive_id())),
                "volume physical ordering conflict"
            );
            slots
        };
        let new_volume_mounts = if restore.is_some() {
            saved_slots
                .iter()
                .enumerate()
                .filter(|(_, slot)| slot.mount_path.is_none())
                .filter_map(|(index, _)| {
                    spec.extra_drives
                        .get(attached_count + index)
                        .cloned()
                        .map(|d| (attached_count + index, d))
                })
                .collect()
        } else {
            spec.extra_drives
                .iter()
                .enumerate()
                .filter(|(_, drive)| drive.is_volume())
                .map(|(index, drive)| (index, drive.clone()))
                .collect()
        };
        if let Some(saved) = &restore {
            let machine = &saved.artifacts.envelope.machine;
            ensure!(
                machine.block_devices.len()
                    == if volume_slots.is_empty() {
                        spec.extra_drives.len() + 2
                    } else {
                        attached_count + volume_slots.len() + 2
                    }
                    && u32::from(machine.vcpu_count) == spec.resources.cpu_count
                    && machine.memory_mib == spec.resources.memory_mib,
                "restore resource/device count conflict"
            );
            for (drive, slot) in spec.extra_drives.iter().zip(&machine.block_devices[2..]) {
                ensure!(
                    !slot.is_root
                        && (if slot.is_volume_slot {
                            (!slot.read_only || drive.read_only())
                                && (saved_slots
                                    .iter()
                                    .find(|s| s.slot_id == slot.drive_id)
                                    .is_some_and(|s| s.mount_path.is_none())
                                    || (Some(slot.capacity_bytes) == drive.virtual_size()
                                        && slot.backing_read_only.unwrap_or(slot.read_only)
                                            == drive.read_only()))
                        } else {
                            slot.read_only == drive.read_only()
                                && Some(slot.capacity_bytes) == drive.virtual_size()
                        })
                        && (drive.is_volume() || slot.drive_id == drive.drive_id()),
                    "current backing changes captured physical topology"
                );
            }
        }
        let policy = &factory.policy;
        let app = &policy.app;
        fs::create_dir_all(&app.runtime_path)?;
        let work = tempfile::Builder::new()
            .prefix("db-runtime-")
            .tempdir_in(&app.runtime_path)?;
        fs::set_permissions(work.path(), fs::Permissions::from_mode(0o700))?;
        let evidence = app
            .runtime_path
            .join("dragonball-evidence")
            .join("failures-v1");
        let mut resources = RuntimeResources::new(work, Some(evidence));
        if let Some(saved) = &restore {
            resources.retained_artifacts.push(saved.artifacts.clone());
        }
        if let Some(recording) = &recording {
            // Template captures borrow their builder workspace. Keep its owner
            // through failed boot/cleanup, including detached startup rollback.
            resources.retained_artifacts.push(recording.lease.clone());
        }
        let instance = uuid::Uuid::new_v4().to_string();
        let socket = resources.work.path().join("runner.sock");
        let network_started = std::time::Instant::now();
        resources.slot = Some(NetworkManager::global().allocate_any()?);
        let slot = resources.slot.as_mut().unwrap();
        slot.set_egress_policy(launch.network.as_ref())?;
        slot.prepare_metadata()?;
        let namespace = slot.namespace_path();
        let metadata = SandboxMetadataDocument::new(launch.sandbox_id, launch.snapshot_id.clone())
            .with_extra(launch.extra_mmds.clone())
            .with_access_token(launch.envd_access_token.as_ref());
        resources.metadata = Some(MetadataListener::start(
            &namespace,
            &instance,
            metadata.clone(),
        )?);
        let envd = EnvdInstance::new(
            format!(
                "http://{}:{}",
                slot.host_interaction_ip, app.tools.control_plane_port
            ),
            launch.envd_access_token.clone(),
        );
        let network_us = network_started.elapsed().as_micros() as u64;
        let mut boot_args = format!(
            "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda ro init=/init {}",
            slot.build_ip_boot_arg()
        );
        boot_args.push(' ');
        boot_args.push_str(&super::damon::boot_args(
            &MemoryLayout::for_runtime_memory(spec.resources.memory_mib),
        )?);
        let boot_drives = if volume_slots.is_empty() {
            &spec.extra_drives[..]
        } else {
            &spec.extra_drives[..attached_count]
        };
        if let Some(args) = build_drives_boot_arg(boot_drives) {
            boot_args.push(' ');
            boot_args.push_str(&args);
        }
        let mut extension = policy
            .extension
            .clone()
            .map(|client| CustomExtensionHookGuard::new(client, launch.sandbox_id));
        if let Some(guard) = &mut extension {
            let args = if restore.is_some() {
                guard
                    .start_resume(
                        namespace.to_str().context("namespace UTF-8")?,
                        slot.host_interaction_ip,
                        launch.custom_extension_params.as_ref(),
                    )
                    .await?;
                None
            } else {
                guard
                    .start_fresh(
                        namespace.to_str().context("namespace UTF-8")?,
                        slot.host_interaction_ip,
                        launch.custom_extension_params.as_ref(),
                    )
                    .await?
            };
            if let Some(args) = args {
                boot_args.push(' ');
                boot_args.push_str(&args);
            }
        }
        // Start the owned runner while disk provisioning can make progress.
        // Configure/LoadHeld still wait for every device; failure below drops
        // RuntimeResources, which reaps the runner before releasing its leases.
        let spawn_started = std::time::Instant::now();
        let mut command = tokio::process::Command::new(&policy.runner);
        command
            .arg("--runtime")
            .arg("--socket")
            .arg(&socket)
            .arg("--work-dir")
            .arg(resources.work.path())
            .arg("--instance-id")
            .arg(&instance)
            .arg("--parent-pid")
            .arg(std::process::id().to_string())
            .current_dir(resources.work.path())
            .stdin(Stdio::null());
        resources.process = Some(
            spawn_logged_runner(
                command,
                Some(File::open(namespace)?),
                resources.work.clone(),
            )
            .await?,
        );
        let spawn_us = spawn_started.elapsed().as_micros() as u64;
        let disks_started = std::time::Instant::now();
        let root = UblkDeviceManager::global()
            .create_overlaybd_runtime_device(CreateOverlaybdRuntimeDeviceRequest {
                source_image_config: &spec.image_config_path,
                global_config: &app.ublk.overlaybd.global_config_path,
                runtime_dir: &resources.work.path().join("overlaybd"),
                read_only: false,
                runtime_upper_mode: app.ublk.overlaybd.runtime_upper_mode,
                requested_virtual_size: (restore.is_none() && spec.resources.disk_size_mib != 0)
                    .then_some(u64::from(spec.resources.disk_size_mib) * 1024 * 1024),
                known_source_virtual_size: restore
                    .as_ref()
                    .map(|saved| saved.artifacts.manifest.rootfs.virtual_size),
                allow_shrink: false,
            })
            .await?;
        let root = OverlaybdRuntimeHandle {
            device: root.device,
            image_config_path: root.image_config_path,
            actual_virtual_size: root.actual_virtual_size,
        };
        resources.devices.push(root.device.clone());
        let mut disks = vec![
            RuntimeBlockDevice {
                drive_id: "rootfs".into(),
                path_on_host: policy.tools.clone(),
                read_only: true,
                is_root: true,
                is_volume_slot: false,
                backing_read_only: None,
                rate_limiter: None,
            },
            RuntimeBlockDevice {
                drive_id: "user_rootfs".into(),
                path_on_host: root.device.device_path().into(),
                read_only: false,
                is_root: false,
                is_volume_slot: false,
                backing_read_only: None,
                rate_limiter: policy.disk_rate.clone(),
            },
        ];
        let prepared = prepare_extra_drives(
            &spec.extra_drives,
            &app.ublk.overlaybd.global_config_path,
            resources.work.path(),
            app.ublk.overlaybd.runtime_upper_mode,
            |_| {
                if restore.is_some() {
                    ExtraDrivePrepareMode::Resume
                } else {
                    ExtraDrivePrepareMode::Fresh {
                        allow_shrink: false,
                    }
                }
            },
        )
        .await?;
        let (mounts, extras) = prepared.into_parts();
        resources
            .devices
            .extend(extras.iter().map(|runtime| runtime.device.clone()));
        for (index, mount) in mounts.into_iter().enumerate() {
            // The runner retains the exact inode rather than the temporary host symlink.
            let path = resources
                .work
                .path()
                .join(&mount.attachment_path)
                .canonicalize()?;
            disks.push(RuntimeBlockDevice {
                // Volume catalog IDs change at fork; the captured physical slot ID does not.
                // Only its checked current backing is rebound, never opaque virtio state.
                drive_id: restore
                    .as_ref()
                    .map(|saved| {
                        saved.artifacts.envelope.machine.block_devices[index + 2]
                            .drive_id
                            .clone()
                    })
                    .unwrap_or_else(|| {
                        if index >= attached_count && !volume_slots.is_empty() {
                            volume_slots[index - attached_count].slot_id.clone()
                        } else {
                            mount.drive_id
                        }
                    }),
                path_on_host: path,
                read_only: if index >= attached_count && !volume_slots.is_empty() {
                    volume_slots[index - attached_count].guest_read_only
                } else {
                    mount.read_only
                },
                is_root: false,
                is_volume_slot: index >= attached_count && !volume_slots.is_empty(),
                backing_read_only: (index >= attached_count
                    && !volume_slots.is_empty()
                    && !volume_slots[index - attached_count].guest_read_only)
                    .then_some(mount.read_only),
                rate_limiter: policy.disk_rate.clone(),
            });
        }
        // Reuse the common 4KiB placeholder format. Bound entries use the real
        // prepared device above; placeholder files remain private runtime scratch.
        let placeholders =
            prepare_volume_drive_slots(attached_count, volume_slots.len(), resources.work.path())?;
        for (index, mount) in placeholders.into_iter().enumerate() {
            if volume_slots[index].mount_path.is_some() {
                continue;
            }
            disks.push(RuntimeBlockDevice {
                drive_id: volume_slots[index].slot_id.clone(),
                path_on_host: resources
                    .work
                    .path()
                    .join(mount.attachment_path)
                    .canonicalize()?,
                read_only: volume_slots[index].guest_read_only,
                is_root: false,
                is_volume_slot: true,
                backing_read_only: Some(false),
                rate_limiter: policy.disk_rate.clone(),
            });
        }
        let disks_us = disks_started.elapsed().as_micros() as u64;
        let provisioned_us = started.elapsed().as_micros() as u64;
        let runner_started = std::time::Instant::now();
        let until = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if resources
                .process
                .as_ref()
                .unwrap()
                .child
                .try_wait()?
                .is_some()
            {
                resources.reap().await?;
                anyhow::bail!(
                    "runner exited before Hello: {}",
                    super::logging::read_tail(&resources.work.path().join("runner-error.log"))
                        .unwrap_or_default()
                );
            }
            if socket.exists() {
                break;
            }
            ensure!(
                tokio::time::Instant::now() < until,
                "runtime socket readiness timed out"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let mut client = RuntimeClient::new(socket, instance);
        client.bind_process(
            resources
                .process
                .as_ref()
                .and_then(|process| process.child.id())
                .context("runner PID missing")?,
        )?;
        let RuntimeReply::Hello(hello) = client
            .request(RuntimeCommand::Hello, Duration::from_secs(5))
            .await?
        else {
            anyhow::bail!("unexpected runtime Hello");
        };
        ensure!(
            hello.runner_build_id == factory.build.runner_build_id
                && hello.dragonball_revision == factory.build.dragonball_revision
                && hello.snapshot_format_epoch == factory.build.snapshot_format_epoch
                && hello.restore_abi_id == factory.build.restore_abi_id
                && hello.feature_profile == factory.build.feature_profile,
            "runtime binary changed after preflight"
        );
        let hello_us = started.elapsed().as_micros() as u64;
        let reader = RestoreReader::new(hello)?;
        let identity = if let Some(saved) = &restore {
            reader.validate_restore(&saved.artifacts.envelope.identity)?;
            let saved_identity = &saved.artifacts.envelope.identity;
            reader.identity(
                saved_identity.kernel_version.clone(),
                saved_identity.tools_drive_version.clone(),
                saved_identity.envd_version.clone(),
            )
        } else {
            reader.identity(
                app.resolved_kernel_version().into(),
                app.resolved_tools_version().into(),
                app.envd.version.clone(),
            )
        };
        let machine = RuntimeVmConfig {
            vcpu_count: spec.resources.cpu_count.try_into()?,
            memory_mib: spec.resources.memory_mib,
            kernel_path: policy.kernel.clone(),
            boot_args,
            block_devices: disks,
            network: RuntimeNetworkDevice {
                iface_id: "eth0".into(),
                tap_name: "tap0".into(),
            },
            identity,
        };
        super::capture::write_new_json(
            &resources.work.path().join("runtime-config.json"),
            &machine,
        )?;
        client
            .request(
                RuntimeCommand::Configure(machine.clone()),
                Duration::from_secs(15),
            )
            .await?;
        let configured_us = started.elapsed().as_micros() as u64;
        let runner_us = spawn_us + runner_started.elapsed().as_micros() as u64;
        let mut memory_ready_us = configured_us;
        let mut memory_us = 0;
        let mut load_us = 0;
        let mut memory_base = None;
        let mut startup_prefetch = None;
        // Metadata listener and configured rate buckets are ready before the first KVM_RUN.
        if let Some(saved) = &restore {
            let memory_started = std::time::Instant::now();
            let state_path = resources.work.path().join("restore-state.json");
            super::capture::write_new_json(&state_path, &saved.artifacts.envelope)?;
            let memory_path = if let Some(recording) = &recording {
                let manager = UblkDeviceManager::global();
                let device = manager
                    .create_dedicated_mem_device(&crate::sandbox::ublk::UblkCreateSpec::Overlaybd {
                        image_config: recording.memory_config.clone(),
                        global_config: app.memory_snapshot.overlaybd_global_config_path.clone(),
                    })
                    .await?;
                let path = device.device_path().to_path_buf();
                let dev_id = device.dev_id();
                // Own deletion before any fallible arm or LoadHeld request.
                resources.dedicated_devices.push(device);
                let config = &app.snapshot.memory_startup_pack;
                manager
                    .start_pack_recording(
                        dev_id,
                        &recording.trace,
                        crate::sandbox::ublk::PackRecordingWindow {
                            max_pages: (config.max_pack_bytes
                                / overlaybd::startup_pack::PACK_PAGE_BYTES)
                                .min(u64::from(overlaybd::startup_pack::MAX_PACK_PAGES))
                                as u32,
                            min_window_ms: config.record_min_window_ms,
                            quiet_ms: config.record_quiet_ms,
                            max_window_ms: config.record_max_window_ms,
                        },
                    )
                    .await?;
                path
            } else {
                match policy.memory_mode {
                    super::config::DragonballMemoryMode::FullFileOracle => {
                        let path = resources.work.path().join("restore-memory.bin");
                        super::capture::materialize_memory(
                            &saved.artifacts.manifest.memory.image_config_path,
                            saved.artifacts.envelope.memory_layout.image_size,
                            &app.ublk.overlaybd.global_config_path,
                            &path,
                        )
                        .await?;
                        path
                    }
                    super::config::DragonballMemoryMode::SharedUblkIncremental => {
                        let global = &app.memory_snapshot.overlaybd_global_config_path;
                        let pack = saved
                            .artifacts
                            .manifest
                            .memory_startup_pack
                            .as_ref()
                            .filter(|_| app.snapshot.memory_startup_pack.consume_enabled);
                        if let Some(pack) = pack {
                            UblkDeviceManager::global()
                                .prefetch_startup_pack(
                                    &saved.artifacts.manifest.memory.image_config_path,
                                    global,
                                    pack,
                                )
                                .await;
                        }
                        let base = super::memory::DragonballMemoryBase::acquire(
                            saved.artifacts.clone(),
                            global,
                        )
                        .await?;
                        let path = base.device_path().to_path_buf();
                        // Arm the reap-before-leases owner before LoadHeld can map RAM.
                        resources.retained_artifacts.push(base.clone());
                        if let Some(pack) = pack {
                            startup_prefetch =
                                Some(crate::sandbox::startup_pack::submit_local_startup_prefetch(
                                    base.device.clone(),
                                    global.clone(),
                                    pack.clone(),
                                    &app.snapshot.memory_startup_pack,
                                    Some(base.clone()),
                                ));
                        }
                        memory_base = Some(base);
                        path
                    }
                }
            };
            memory_ready_us = started.elapsed().as_micros() as u64;
            memory_us = memory_started.elapsed().as_micros() as u64;
            let load_started = std::time::Instant::now();
            client
                .request(
                    RuntimeCommand::LoadHeld(RuntimeSnapshotInput {
                        state_path,
                        memory_path,
                        identity: saved.artifacts.envelope.identity.clone(),
                        probe_only: false,
                    }),
                    Duration::from_secs(30),
                )
                .await?;
            load_us = load_started.elapsed().as_micros() as u64;
        } else {
            client
                .request(RuntimeCommand::Start, Duration::from_secs(30))
                .await?;
        }
        let post_load_started = std::time::Instant::now();
        let pid = resources
            .process
            .as_ref()
            .and_then(|process| process.child.id())
            .context("runner PID missing")?;
        verify_runner_security(pid)?;
        if restore.is_some() {
            tracing::debug!(
                target: "agentenv::sandbox::startup_timing",
                backend = "dragonball", sandbox_id = %launch.sandbox_id,
                network_us, disks_us, runner_us, memory_us, load_us,
                post_load_us = post_load_started.elapsed().as_micros() as u64,
                total_us = started.elapsed().as_micros() as u64,
                "restore preparation phase timing"
            );
        }
        tracing::debug!(
            sandbox_id = %launch.sandbox_id,
            restored = restore.is_some(),
            provisioned_us, hello_us, configured_us, memory_ready_us,
            started_us = started.elapsed().as_micros() as u64,
            "Dragonball boot cumulative timing"
        );
        Ok(RuntimeStarted {
            resources,
            client: Arc::new(tokio::sync::Mutex::new(client)),
            envd,
            pid,
            machine,
            memory_base,
            startup_prefetch,
            root: Some(root),
            extras,
            volume_slots,
            new_volume_mounts,
            ready: AtomicBool::new(false),
            readiness_failed: AtomicBool::new(false),
            extension,
            frozen_volume_mounts: vec![],
        })
    }

    async fn wait_for_ready_inner(&self) -> Result<()> {
        let runtime = self.live_runtime()?;
        if runtime.ready.load(Ordering::Acquire) {
            return Ok(());
        }
        let started = std::time::Instant::now();
        let app = &self.factory.policy.app;
        {
            let mut client = runtime.client.lock().await;
            let RuntimeReply::Status(status) = client
                .request(RuntimeCommand::Status, Duration::from_secs(5))
                .await?
            else {
                anyhow::bail!("unexpected runtime Status");
            };
            if status.phase == RuntimePhase::LoadedHeld {
                ensure!(
                    runtime.resources.metadata.is_some(),
                    "metadata must be ready before held release"
                );
                verify_runner_security(runtime.pid)?;
                let token = status.capture.context("missing loaded-held proof")?.token;
                client
                    .request(RuntimeCommand::Resume { token }, Duration::from_secs(30))
                    .await?;
            } else {
                ensure!(
                    status.phase == RuntimePhase::Running,
                    "runtime is not running or loaded-held"
                );
            }
        }
        let resumed_us = started.elapsed().as_micros() as u64;
        let budget = Duration::from_secs(app.envd.init_timeout_secs);
        runtime
            .envd
            .wait_for_ready(budget, Duration::from_millis(app.envd.poll_ms))
            .await
            .with_context(|| {
                format!(
                    "envd not ready; serial: {}",
                    super::logging::read_tail(&runtime.resources.work.path().join("serial.log"))
                        .unwrap_or_default()
                )
            })?;
        let envd_us = started.elapsed().as_micros() as u64;
        // Release the actual daemon image keys once envd can serve requests.
        // The recorder uses its own image config; FullFileOracle has no daemon RAM device.
        let manager = UblkDeviceManager::global();
        if let Some(root) = &runtime.root {
            manager.notify_sandbox_ready(&root.image_config_path).await;
        }
        if let Some(recording) = &self.recording {
            manager.notify_sandbox_ready(&recording.memory_config).await;
        } else if let Some(base) = &runtime.memory_base {
            manager
                .notify_sandbox_ready(&base.artifacts.manifest.memory.image_config_path)
                .await;
        }
        let daemon_notified_us = started.elapsed().as_micros() as u64;
        let mut vars = self.spec.context.env_vars.clone();
        if let Some(overrides) = &self.launch.env_vars {
            vars.extend(overrides.clone());
        }
        // Initialize the final context once. Internal readiness/mount commands
        // select root and / explicitly, independently of USER and WORKDIR.
        // Preserve the bootstrap defaults for empty USER/WORKDIR as well.
        let user = self
            .spec
            .context
            .user
            .clone()
            .filter(|user| !user.is_empty())
            .unwrap_or_else(|| "root".into());
        let workdir = if self.spec.context.workdir.is_empty() {
            "/"
        } else {
            &self.spec.context.workdir
        };
        tokio::time::timeout(
            budget,
            runtime.envd.init(
                (!vars.is_empty()).then_some(vars),
                Some(workdir.into()),
                Some(user),
            ),
        )
        .await??;
        let initialized_us = started.elapsed().as_micros() as u64;
        let executor = Executor::new(runtime.envd.clone()).with_root_user();
        let mut attempts = 0usize;
        // New instances must replace the identity captured in guest memory.
        // `/envs` reads the same defaults map that envd uses for new processes;
        // spawning a shell adds process setup and page faults to this check.
        // Same-ID paused restore retains its identity and needs no probe.
        if self.require_guest_identity {
            let sandbox_id = self.launch.sandbox_id.to_string();
            let mut completed = 0usize;
            let identity_started = tokio::time::Instant::now();
            tokio::time::timeout(budget, async {
                loop {
                    attempts += 1;
                    let matches = runtime
                        .envd
                        .guest_identity_matches(&sandbox_id, &self.launch.snapshot_id)
                        .await?;
                    completed += 1;
                    if matches {
                        return Ok::<_, anyhow::Error>(());
                    }
                    // envd refreshes metadata asynchronously after /init. Probe
                    // promptly during that first refresh, then retain the slower
                    // cadence if metadata stays unavailable. The deadline and
                    // exact identity check remain unchanged.
                    let retry_ms = if identity_started.elapsed() < Duration::from_millis(100) {
                        5
                    } else {
                        50
                    };
                    tokio::time::sleep(Duration::from_millis(retry_ms)).await;
                }
            })
            .await
            .with_context(|| {
                format!(
                    "envd did not acquire current runtime identity: attempts={attempts}, completed={completed}"
                )
            })??;
        }
        let identity_us = started.elapsed().as_micros() as u64;
        for (index, drive) in &runtime.new_volume_mounts {
            crate::sandbox::guest::mount_volume(&executor, *index, drive).await?;
        }
        verify_runner_security(runtime.pid)?;
        runtime.ready.store(true, Ordering::Release);
        tracing::debug!(
            sandbox_id = %self.launch.sandbox_id,
            resumed_us, envd_us, initialized_us,
            daemon_notified_us, identity_us, attempts,
            identity_required = self.require_guest_identity,
            ready_us = started.elapsed().as_micros() as u64,
            "Dragonball readiness cumulative timing"
        );
        Ok(())
    }

    pub(super) async fn stop_runtime(&mut self, mut archive_failure: bool) -> Result<()> {
        let Some(runtime) = self.runtime.as_mut() else {
            return Ok(());
        };
        archive_failure |= runtime.readiness_failed.load(Ordering::Acquire);
        runtime.ready.store(false, Ordering::Release);
        if let Some(prefetch) = runtime.startup_prefetch.take() {
            prefetch.stop(Duration::from_secs(5)).await;
        }
        runtime.envd.invalidate();
        if let Err(error) = runtime
            .client
            .lock()
            .await
            .request(RuntimeCommand::Stop, Duration::from_secs(10))
            .await
        {
            archive_failure = true;
            tracing::warn!(%error, "runtime Stop not confirmed; requiring process reap");
        }
        if let Some(process) = runtime.resources.process.as_mut() {
            process.terminate_and_wait(Duration::from_secs(10)).await?;
        }
        if archive_failure {
            runtime.resources.archive_failure();
        }
        if let Some(extension) = runtime.extension.take() {
            extension.stop().await;
        }
        runtime.resources.cleanup().await?;
        self.runtime = None;
        Ok(())
    }

    fn live_runtime(&self) -> Result<&RuntimeStarted> {
        let runtime = self
            .runtime
            .as_ref()
            .context("Dragonball sandbox not running")?;
        let status = fs::read_to_string(format!("/proc/{}/status", runtime.pid))
            .context("Dragonball runner exited")?;
        ensure!(
            !status
                .lines()
                .any(|line| line.starts_with("State:") && line.contains('Z')),
            "Dragonball runner exited"
        );
        Ok(runtime)
    }
}

fn verify_runner_security(pid: u32) -> Result<()> {
    let mut count = 0;
    for entry in fs::read_dir(format!("/proc/{pid}/task"))? {
        let status = fs::read_to_string(entry?.path().join("status"))?;
        for (key, expected) in [
            ("CapEff:", "0000000000000000"),
            ("CapPrm:", "0000000000000000"),
            ("CapAmb:", "0000000000000000"),
            ("NoNewPrivs:", "1"),
            ("Seccomp:", "2"),
        ] {
            ensure!(
                status
                    .lines()
                    .find_map(|line| line.strip_prefix(key))
                    .map(str::trim)
                    == Some(expected),
                "runner thread violates {key} requirement"
            );
        }
        count += 1;
    }
    ensure!(count != 0, "runner has no live threads");
    Ok(())
}

#[async_trait(?Send)]
impl SandboxExecutor for DragonballSandbox {
    fn executor(&self) -> Result<Executor> {
        let runtime = self.live_runtime()?;
        ensure!(
            runtime.ready.load(Ordering::Acquire),
            "Dragonball sandbox is not Ready"
        );
        Ok(Executor::new(runtime.envd.clone()))
    }
}

#[async_trait]
impl SandboxBackend for DragonballSandbox {
    fn envd(&self) -> Option<Arc<dyn crate::sandbox::EnvdApi>> {
        self.runtime
            .as_ref()
            .map(|runtime| Arc::new(runtime.envd.clone()) as Arc<dyn crate::sandbox::EnvdApi>)
    }

    async fn start(&mut self) -> Result<()> {
        self.start_nowait().await?;
        if let Err(error) = self.wait_for_ready().await {
            if let Err(cleanup) = self.stop_runtime(true).await {
                tracing::error!(%cleanup, "Dragonball readiness rollback incomplete");
            }
            return Err(error);
        }
        Ok(())
    }
    async fn start_nowait(&mut self) -> Result<()> {
        ensure!(self.runtime.is_none(), "Dragonball sandbox already started");
        // Detached allocation completes and owns rollback even if the requester disappears.
        self.runtime = Some(
            tokio::spawn(Self::boot(
                self.factory.clone(),
                self.spec.clone(),
                self.launch.clone(),
                self.restore.clone(),
                self.recording.clone(),
            ))
            .await
            .context("Dragonball startup owner failed")??,
        );
        Ok(())
    }
    async fn wait_for_ready(&self) -> Result<()> {
        let result = self.wait_for_ready_inner().await;
        if result.is_err() {
            if let Some(runtime) = &self.runtime {
                runtime.readiness_failed.store(true, Ordering::Release);
            }
        }
        result
    }
    async fn pause(
        &mut self,
        root: Option<&Path>,
    ) -> SandboxCaptureResult<Arc<dyn PausedSandboxState>> {
        self.capture_paused(root)
            .await
            .map(|state| state as Arc<dyn PausedSandboxState>)
    }
    async fn resume(&mut self) -> Result<()> {
        self.resume_capture_owned().await
    }
    async fn snapshot(&mut self) -> SandboxCaptureResult<CapturedSandboxSnapshot> {
        let artifacts = self.capture_owned(None, None, false).await?;
        let volumes = artifacts
            .spec
            .extra_drives
            .iter()
            .filter(|drive| drive.is_volume())
            .cloned()
            .collect();
        Ok(
            CapturedSandboxSnapshot::new(artifacts.manifest.clone(), artifacts)
                .with_recording_factory(super::startup_pack::startup_recording)
                .with_volume_drives(volumes),
        )
    }
    async fn snapshot_volumes(&mut self) -> SandboxCaptureResult<()> {
        self.volume_operation(super::volume::VolumeOperation::Snapshot)
            .await
    }
    async fn fork(
        &mut self,
        specs: &[SandboxForkSpec],
    ) -> SandboxCaptureResult<Vec<SandboxForkResult>> {
        if specs.is_empty() {
            return Ok(vec![]);
        }
        let artifacts = self.capture_owned(None, None, false).await?;
        let factory = self.factory.clone();
        let specs = specs.to_vec();
        // Source is confirmed Running before child allocation. The owner continues cleanup
        // even when the caller drops its future; every result stays in request order.
        tokio::spawn(async move {
            futures::future::join_all(specs.into_iter().map(|child| {
                let factory = factory.clone();
                let artifacts = artifacts.clone();
                async move {
                    let mut sandbox = factory.build_fork(artifacts, child).await?;
                    if let Err(error) = sandbox.start().await {
                        if let Err(cleanup) = sandbox.stop().await {
                            tracing::error!(%cleanup, "failed fork child teardown incomplete");
                        }
                        return Err(error);
                    }
                    Ok(Box::new(sandbox) as Box<dyn SandboxBackend>)
                }
            }))
            .await
        })
        .await
        .map_err(|error| SandboxCaptureError::recoverable(error.into()))
    }
    async fn capture_to_dir(&mut self, at: &Path) -> SandboxCaptureResult<CapturedSandboxSnapshot> {
        let artifacts = self.capture_owned(Some(at.into()), None, true).await?;
        let volumes = artifacts
            .spec
            .extra_drives
            .iter()
            .filter(|drive| drive.is_volume())
            .cloned()
            .collect();
        Ok(
            CapturedSandboxSnapshot::new(artifacts.manifest.clone(), artifacts)
                .with_volume_drives(volumes)
                .with_recording_factory(super::startup_pack::startup_recording),
        )
    }
    async fn stop(&mut self) -> Result<()> {
        self.stop_runtime(false).await
    }
    async fn freeze_and_snapshot_volumes(&mut self) -> SandboxCaptureResult<()> {
        self.volume_operation(super::volume::VolumeOperation::Freeze)
            .await
    }
    async fn thaw_volumes(&mut self) -> Result<()> {
        self.volume_operation(super::volume::VolumeOperation::Thaw)
            .await
            .map_err(Into::into)
    }
    fn host_interaction_ip(&self) -> Option<Ipv4Addr> {
        self.runtime
            .as_ref()
            .and_then(|runtime| runtime.resources.slot.as_ref())
            .map(|slot| slot.host_interaction_ip)
    }
    fn runtime_info(&self) -> SandboxRuntimeInfo {
        SandboxRuntimeInfo {
            rootfs_virtual_size: self
                .runtime
                .as_ref()
                .and_then(|runtime| runtime.root.as_ref())
                .map(|root| root.actual_virtual_size),
            runtime_artifacts: self.startup_artifacts(),
        }
    }
    fn startup_artifacts(&self) -> RuntimeArtifactSet {
        let mut paths = vec![self.spec.image_config_path.clone()];
        if let Some(saved) = &self.restore {
            paths.push(saved.artifacts.manifest.memory.image_config_path.clone());
        }
        paths.extend(
            self.spec
                .extra_drives
                .iter()
                .map(|drive| drive.image_config_path().into()),
        );
        if let Some(runtime) = &self.runtime {
            paths.extend(
                runtime
                    .root
                    .iter()
                    .chain(&runtime.extras)
                    .map(|root| root.image_config_path.clone()),
            );
        }
        RuntimeArtifactSet::from_overlaybd_image_configs(paths)
    }
    async fn update_network_policy(&mut self, policy: Option<SandboxNetworkPolicy>) -> Result<()> {
        if let Some(runtime) = &mut self.runtime {
            runtime
                .resources
                .slot
                .as_mut()
                .context("runtime network slot missing")?
                .set_egress_policy(policy.as_ref())?;
        }
        self.launch.network = policy;
        Ok(())
    }
    fn update_custom_extension_params(&mut self, params: Option<CustomExtensionParams>) {
        self.launch.custom_extension_params = params;
    }
}
