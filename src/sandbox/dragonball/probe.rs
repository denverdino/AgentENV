use std::{
    fs::{self, File},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use agentenv_vmm_protocol::{
    ProbeBlockDevice, ProbeCommand, ProbeNetworkDevice, ProbeReply, ProbeVmConfig, PROTOCOL_VERSION,
};
use anyhow::{Context, Result};
use tempfile::TempDir;
use uvm_ublk_daemon::CreateOverlaybdRuntimeDeviceRequest;

use super::{
    client::{ProbeClient, M0_PROFILE, M0_REVISION},
    process::spawn_runner,
    resources::{RetainedWork, RuntimeResources},
};
use crate::{
    cfg::ConfigManager,
    sandbox::{
        envd::EnvdInstance,
        extra_drive::{
            build_drives_boot_arg, prepare_extra_drives, prepare_volume_drive_slots,
            ExtraDrivePrepareMode,
        },
        network::metadata::MetadataListener,
        ublk::UblkDevice,
        EnvdAccessToken, Executor, ExtraDrive, NetworkManager, SandboxMetadataDocument,
        SandboxNetworkPolicy, UblkDeviceManager,
    },
    virtualization::VirtualizationMode,
};

#[derive(Clone)]
pub struct DragonballM0ProbeConfig {
    pub runner_path: PathBuf,
    pub kernel_path: PathBuf,
    pub tools_path: PathBuf,
    pub image_path: PathBuf,
    pub work_root: PathBuf,
    pub vcpu_count: u8,
    pub memory_mib: u32,
    pub sandbox_instance_id: String,
    pub metadata: SandboxMetadataDocument,
    pub envd_access_token: EnvdAccessToken,
    pub extra_drives: Vec<ExtraDrive>,
    pub network_policy: SandboxNetworkPolicy,
    pub virtualization_mode: VirtualizationMode,
    pub cpu_policy: String,
}

pub struct DragonballM0Probe {
    resources: Option<ProbeResources>,
    client: ProbeClient,
    envd: Option<EnvdInstance>,
    address: String,
    machine: Option<ProbeVmConfig>,
    config: Option<DragonballM0ProbeConfig>,
}

#[derive(Clone)]
pub struct QuiescedProbeArtifacts {
    pub state_path: PathBuf,
    pub memory_path: PathBuf,
    machine: ProbeVmConfig,
    config: DragonballM0ProbeConfig,
    backing: Arc<ProbeBacking>,
    directory: Arc<TempDir>,
}

struct ProbeBacking {
    devices: Vec<UblkDevice>,
    work: Arc<TempDir>,
}
struct ProbeResources {
    runtime: RuntimeResources,
    backing: Option<Arc<ProbeBacking>>,
    evidence_dir: PathBuf,
}

fn validate_profile(mode: VirtualizationMode, arch: &str, cpu_policy: &str) -> Result<()> {
    anyhow::ensure!(
        mode == VirtualizationMode::Kvm,
        "M0 only qualifies KVM, not {mode}"
    );
    anyhow::ensure!(arch == "x86_64", "M0 only qualifies x86_64");
    anyhow::ensure!(
        cpu_policy == "host-default",
        "M0 does not qualify CPU policy {cpu_policy}"
    );
    Ok(())
}

impl DragonballM0Probe {
    pub fn machine_config(&self) -> Result<&ProbeVmConfig> {
        self.machine.as_ref().context("probe not configured")
    }
    pub fn work_dir(&self) -> Result<&std::path::Path> {
        Ok(self
            .resources
            .as_ref()
            .context("probe stopped")?
            .runtime
            .work
            .path())
    }
    pub async fn pause_probe(&mut self) -> Result<()> {
        self.client
            .request(ProbeCommand::Pause, Duration::from_secs(5))
            .await?;
        if let Some(envd) = self.envd.take() {
            envd.invalidate();
        }
        Ok(())
    }
    pub async fn save_quiesced(
        &mut self,
        output_dir: &std::path::Path,
    ) -> Result<QuiescedProbeArtifacts> {
        let resources = self.resources.as_mut().context("probe stopped")?;
        let parent = output_dir
            .parent()
            .context("checkpoint parent")?
            .canonicalize()?;
        anyhow::ensure!(
            output_dir.is_absolute() && parent.starts_with(resources.runtime.work.path()),
            "checkpoint must be within this instance's work root"
        );
        fs::create_dir(output_dir)?;
        fs::set_permissions(output_dir, fs::Permissions::from_mode(0o700))?;
        let state_path = output_dir.join("state.json");
        let memory_path = output_dir.join("memory.bin");
        self.client
            .request(
                ProbeCommand::SaveQuiesced {
                    state_path: state_path.clone(),
                    memory_path: memory_path.clone(),
                },
                Duration::from_secs(30),
            )
            .await?;
        let backing = resources
            .backing
            .get_or_insert_with(|| {
                let backing = Arc::new(ProbeBacking {
                    devices: std::mem::take(&mut resources.runtime.devices),
                    work: resources.runtime.work.clone(),
                });
                resources.runtime.retained_artifacts.push(backing.clone());
                backing
            })
            .clone();
        Ok(QuiescedProbeArtifacts {
            state_path,
            memory_path,
            machine: self.machine.clone().context("machine not configured")?,
            config: self.config.clone().context("probe inputs missing")?,
            backing,
            directory: resources.runtime.work.clone(),
        })
    }
    pub async fn restore_quiesced(
        config: DragonballM0ProbeConfig,
        artifacts: &QuiescedProbeArtifacts,
    ) -> Result<Self> {
        anyhow::ensure!(
            config.vcpu_count == artifacts.config.vcpu_count
                && config.memory_mib == artifacts.config.memory_mib
                && config.kernel_path == artifacts.config.kernel_path
                && config.tools_path == artifacts.config.tools_path
                && config.image_path == artifacts.config.image_path
                && config.extra_drives == artifacts.config.extra_drives,
            "restore machine/authorized disk inputs mismatch"
        );
        for path in [&artifacts.state_path, &artifacts.memory_path] {
            anyhow::ensure!(
                fs::symlink_metadata(path)?.file_type().is_file()
                    && path.canonicalize()?.starts_with(artifacts.directory.path()),
                "unauthorized probe restore artifact"
            );
        }
        Self::start_internal(config, Some(artifacts)).await
    }
    pub async fn start(config: DragonballM0ProbeConfig) -> Result<Self> {
        Self::start_internal(config, None).await
    }

    async fn start_internal(
        config: DragonballM0ProbeConfig,
        restore: Option<&QuiescedProbeArtifacts>,
    ) -> Result<Self> {
        let restore = restore.cloned();
        owned_startup(async move { Self::start_owned(config, restore.as_ref()).await }).await
    }

    async fn start_owned(
        mut config: DragonballM0ProbeConfig,
        restore: Option<&QuiescedProbeArtifacts>,
    ) -> Result<Self> {
        // Profile/shape checks precede allocation, including global managers.
        validate_profile(
            config.virtualization_mode,
            std::env::consts::ARCH,
            &config.cpu_policy,
        )?;
        anyhow::ensure!(
            config.vcpu_count > 0 && config.memory_mib > 0,
            "M0 CPU and memory must be nonzero"
        );
        anyhow::ensure!(
            !config.sandbox_instance_id.is_empty(),
            "M0 instance ID must be nonempty"
        );
        anyhow::ensure!(
            config.extra_drives.len() <= 24,
            "too many physical extra drives"
        );
        let mut ids = std::collections::HashSet::new();
        for drive in &config.extra_drives {
            crate::sandbox::validate_drive_id(drive.drive_id())?;
            anyhow::ensure!(ids.insert(drive.drive_id()), "duplicate extra drive ID");
            anyhow::ensure!(
                !drive.is_volume(),
                "M0 does not acquire managed volume reservations"
            );
        }
        for path in [
            &config.runner_path,
            &config.kernel_path,
            &config.tools_path,
            &config.image_path,
        ] {
            anyhow::ensure!(
                path.is_absolute() && path.is_file(),
                "missing absolute M0 input {}",
                path.display()
            );
        }
        anyhow::ensure!(
            config.work_root.is_absolute(),
            "M0 work root must be absolute"
        );
        fs::create_dir_all(&config.work_root)?;
        let work = tempfile::Builder::new()
            .prefix("db-m0-")
            .tempdir_in(&config.work_root)?;
        fs::set_permissions(work.path(), fs::Permissions::from_mode(0o700))?;
        let socket = work.path().join("runner.sock");
        let evidence_dir = config
            .work_root
            .join("evidence")
            .join(work.path().file_name().context("probe work directory")?);
        let mut probe = Self {
            resources: Some(ProbeResources {
                runtime: RuntimeResources::new(work, None),
                backing: None,
                evidence_dir,
            }),
            client: ProbeClient::new(socket, config.sandbox_instance_id.clone()),
            envd: None,
            address: String::new(),
            machine: None,
            config: Some(config.clone()),
        };
        config
            .metadata
            .set_access_token(Some(&config.envd_access_token));
        if let Err(error) = probe.boot(&config, restore).await {
            if let Err(cleanup) = probe.stop().await {
                tracing::error!(%cleanup, "M0 rollback incomplete");
            }
            return Err(error);
        }
        Ok(probe)
    }

    async fn boot(
        &mut self,
        config: &DragonballM0ProbeConfig,
        restore: Option<&QuiescedProbeArtifacts>,
    ) -> Result<()> {
        let app = ConfigManager::global_config();
        let resources = self.resources.as_mut().context("probe resources missing")?;
        resources.runtime.slot = Some(NetworkManager::global().allocate_any()?);
        let slot = resources.runtime.slot.as_mut().unwrap();
        slot.set_egress_policy(Some(&config.network_policy))?;
        slot.prepare_metadata()?;
        let netns_path = slot.namespace_path();
        resources.runtime.metadata = Some(MetadataListener::start(
            &netns_path,
            &config.sandbox_instance_id,
            config.metadata.clone(),
        )?);
        self.address = format!(
            "http://{}:{}",
            slot.host_interaction_ip, app.tools.control_plane_port
        );
        let mut boot_args = format!(
            "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda ro init=/init {}",
            slot.build_ip_boot_arg()
        );
        let disks = if let Some(artifacts) = restore {
            resources.backing = Some(artifacts.backing.clone());
            resources
                .runtime
                .retained_artifacts
                .push(artifacts.backing.clone());
            resources
                .runtime
                .retained_artifacts
                .push(artifacts.directory.clone());
            artifacts.machine.block_devices.clone()
        } else {
            let root = UblkDeviceManager::global()
                .create_overlaybd_runtime_device(CreateOverlaybdRuntimeDeviceRequest {
                    source_image_config: &config.image_path,
                    global_config: &app.ublk.overlaybd.global_config_path,
                    runtime_dir: &resources.runtime.work.path().join("overlaybd"),
                    read_only: false,
                    runtime_upper_mode: app.ublk.overlaybd.runtime_upper_mode,
                    requested_virtual_size: None,
                    known_source_virtual_size: None,
                    allow_shrink: false,
                })
                .await?;
            let mut disks = vec![
                ProbeBlockDevice {
                    drive_id: "rootfs".into(),
                    path_on_host: config.tools_path.clone(),
                    read_only: true,
                    is_root: true,
                },
                ProbeBlockDevice {
                    drive_id: "user_rootfs".into(),
                    path_on_host: root.device.device_path().to_owned(),
                    read_only: false,
                    is_root: false,
                },
            ];
            resources.runtime.devices.push(root.device);
            let prepared = prepare_extra_drives(
                &config.extra_drives,
                &app.ublk.overlaybd.global_config_path,
                resources.runtime.work.path(),
                app.ublk.overlaybd.runtime_upper_mode,
                ExtraDrivePrepareMode::Fresh {
                    allow_shrink: false,
                },
            )
            .await?;
            let (mounts, runtimes) = prepared.into_parts();
            resources
                .runtime
                .devices
                .extend(runtimes.into_iter().map(|runtime| runtime.device));
            let slot_count = app.volume.max_volume_count.min(24 - mounts.len());
            for mount in mounts.into_iter().chain(prepare_volume_drive_slots(
                config.extra_drives.len(),
                slot_count,
                resources.runtime.work.path(),
            )?) {
                disks.push(ProbeBlockDevice {
                    drive_id: mount.drive_id,
                    path_on_host: resources.runtime.work.path().join(mount.attachment_path),
                    read_only: mount.read_only,
                    is_root: false,
                });
            }
            disks
        };
        if let Some(args) = build_drives_boot_arg(&config.extra_drives) {
            boot_args.push(' ');
            boot_args.push_str(&args);
        }
        let mut command = tokio::process::Command::new(&config.runner_path);
        command
            .arg("--socket")
            .arg(&self.client.socket)
            .arg("--work-dir")
            .arg(resources.runtime.work.path())
            .arg("--instance-id")
            .arg(&config.sandbox_instance_id)
            .arg("--parent-pid")
            .arg(std::process::id().to_string())
            .current_dir(resources.runtime.work.path())
            .stdin(Stdio::null())
            .stdout(File::create(
                resources.runtime.work.path().join("serial.log"),
            )?)
            .stderr(File::create(
                resources.runtime.work.path().join("runner-error.log"),
            )?);
        let namespace = File::open(netns_path)?;
        // Current UID/groups; the runner inherits no capabilities.
        resources.runtime.process = Some(spawn_runner(command, Some(namespace)).await?);
        let until = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if resources
                .runtime
                .process
                .as_mut()
                .unwrap()
                .child
                .try_wait()?
                .is_some()
            {
                anyhow::bail!(
                    "runner exited before Hello: {}",
                    fs::read_to_string(resources.runtime.work.path().join("runner-error.log"))
                        .unwrap_or_default()
                );
            }
            if self.client.socket.exists() {
                break;
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < until,
                "runner socket readiness timed out"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let hello = self
            .client
            .request(ProbeCommand::Hello, Duration::from_secs(5))
            .await?;
        let ProbeReply::Hello(identity) = hello else {
            anyhow::bail!("unexpected Hello reply");
        };
        anyhow::ensure!(
            identity.protocol_version == PROTOCOL_VERSION
                && identity.dragonball_revision == M0_REVISION
                && identity.feature_profile == M0_PROFILE,
            "unqualified runner identity {identity:?}"
        );
        tracing::info!(?identity, uid = unsafe { libc::geteuid() }, work = %resources.runtime.work.path().display(), "M0 runner ready");
        let machine = ProbeVmConfig {
            vcpu_count: config.vcpu_count,
            memory_mib: config.memory_mib,
            kernel_path: config.kernel_path.clone(),
            boot_args,
            block_devices: disks,
            network: ProbeNetworkDevice {
                iface_id: "eth0".into(),
                tap_name: "tap0".into(),
            },
        };
        if let Some(artifacts) = restore {
            anyhow::ensure!(
                machine == artifacts.machine,
                "restore requires the original network/machine layout"
            );
        }
        self.machine = Some(machine.clone());
        self.client
            .request(ProbeCommand::Configure(machine), Duration::from_secs(15))
            .await?;
        tracing::info!(restore = restore.is_some(), work = %resources.runtime.work.path().display(), "M0 guest start requested");
        self.client
            .request(
                if let Some(artifacts) = restore {
                    ProbeCommand::RestoreQuiesced {
                        state_path: artifacts.state_path.clone(),
                        memory_path: artifacts.memory_path.clone(),
                    }
                } else {
                    ProbeCommand::Start
                },
                Duration::from_secs(30),
            )
            .await?;
        let envd = EnvdInstance::new(self.address.clone(), Some(config.envd_access_token.clone()));
        self.envd = Some(envd.clone());
        envd.wait_for_ready(Duration::from_secs(30), Duration::from_millis(50))
            .await
            .with_context(|| {
                format!(
                    "guest not ready; serial: {}",
                    fs::read_to_string(resources.runtime.work.path().join("serial.log"))
                        .unwrap_or_default()
                )
            })?;
        tokio::time::timeout(
            Duration::from_secs(30),
            envd.init(None, Some("/".into()), Some("root".into())),
        )
        .await??;
        // /init starts envd's MMDS poll asynchronously. Health and a successful
        // init response do not prove the process defaults carry this identity.
        let executor = Executor::new(envd).with_root_user();
        let expected = format!(
            "{}\n{}",
            config.metadata.sandbox_id, config.metadata.snapshot_id
        );
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let output = executor
                    .run_command(
                        "/bin/sh",
                        &[
                            "-ec",
                            "printf '%s\\n%s' \"$E2B_SANDBOX_ID\" \"$E2B_TEMPLATE_ID\"",
                        ],
                    )
                    .await?;
                anyhow::ensure!(output.exit_code == 0, "guest identity probe failed");
                if output.stdout == expected {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .context("envd did not acquire this instance's metadata identity")??;
        let pid = resources
            .runtime
            .process
            .as_ref()
            .and_then(|p| p.child.id())
            .context("runner exited before guest readiness")?;
        fs::create_dir_all(&resources.evidence_dir)?;
        let mut security = Vec::new();
        for thread in fs::read_dir(format!("/proc/{pid}/task"))? {
            let status = fs::read_to_string(thread?.path().join("status"))?;
            for (key, expected) in [
                ("CapEff:", "0000000000000000"),
                ("CapPrm:", "0000000000000000"),
                ("CapAmb:", "0000000000000000"),
                ("NoNewPrivs:", "1"),
                ("Seccomp:", "2"),
            ] {
                anyhow::ensure!(
                    status
                        .lines()
                        .find_map(|line| line.strip_prefix(key))
                        .map(str::trim)
                        == Some(expected),
                    "runner thread violates {key} requirement"
                );
            }
            security.push(status);
        }
        anyhow::ensure!(!security.is_empty(), "runner has no live threads");
        fs::write(
            resources.evidence_dir.join("runner-security.txt"),
            security.join("\n"),
        )?;
        tracing::info!(work = %resources.runtime.work.path().display(), "M0 guest ready");
        Ok(())
    }

    pub fn executor(&self) -> Result<Executor> {
        anyhow::ensure!(self.resources.is_some(), "M0 probe stopped");
        Ok(Executor::new(self.envd.clone().context("M0 guest not ready")?).with_root_user())
    }

    /// Qualification endpoint for independently checking rejected credentials.
    pub fn envd_address(&self) -> Result<String> {
        anyhow::ensure!(
            self.resources.is_some() && self.envd.is_some(),
            "M0 guest not ready"
        );
        Ok(self.address.clone())
    }

    pub async fn stop(&mut self) -> Result<()> {
        if let Some(resources) = &self.resources {
            tracing::info!(work = %resources.runtime.work.path().display(), "M0 runner stop requested");
        }
        if let Some(envd) = self.envd.take() {
            envd.invalidate();
        }
        if self
            .resources
            .as_mut()
            .and_then(|r| r.runtime.process.as_mut())
            .is_some_and(|p| matches!(p.child.try_wait(), Ok(None)))
        {
            // Best-effort orderly worker shutdown, followed by mandatory wait/kill.
            let _ = self
                .client
                .request(ProbeCommand::Stop, Duration::from_secs(2))
                .await;
        }
        let Some(mut resources) = self.resources.take() else {
            return Ok(());
        };
        // Caller cancellation detaches the task, retaining leases through exit.
        tokio::spawn(async move { resources.cleanup().await }).await?
    }
}

impl Drop for DragonballM0Probe {
    fn drop(&mut self) {
        if let Some(envd) = &self.envd {
            envd.invalidate();
        }
    }
}

impl ProbeResources {
    async fn cleanup(&mut self) -> Result<()> {
        self.runtime.reap().await?;
        self.runtime.archive_diagnostics(&self.evidence_dir);
        self.runtime.cleanup().await
    }
}

impl Drop for ProbeBacking {
    fn drop(&mut self) {
        let devices = std::mem::take(&mut self.devices);
        let mut work = RetainedWork(Some(self.work.clone()));
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                for device in devices {
                    if let Err(error) = UblkDeviceManager::global().release_device(&device).await {
                        tracing::error!(%error,"checkpoint backing release failed; retaining work");
                        return;
                    }
                }
                work.0.take();
            });
        } else {
            std::mem::forget(devices);
        }
    }
}

async fn owned_startup<T: Send + 'static>(
    operation: impl std::future::Future<Output = Result<T>> + Send + 'static,
) -> Result<T> {
    // The daemon completes allocation independently of its awaiting client.
    // Keep the entire startup owner alive until all allocations are registered
    // or rolled back. If the caller cancels, the detached task's unused output
    // is dropped only after startup finishes, invoking the ordinary cleanup.
    tokio::spawn(operation)
        .await
        .context("M0 startup task failed")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct AllocationLease(Arc<std::sync::atomic::AtomicUsize>);

    impl Drop for AllocationLease {
        fn drop(&mut self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    async fn cancel_startup_during_allocation(prior_devices: usize) {
        let work = tempfile::tempdir().unwrap();
        let work_path = work.path().to_owned();
        let released = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        let (allocation_tx, allocation_rx) = tokio::sync::oneshot::channel();
        let (registered_tx, registered_rx) = tokio::sync::oneshot::channel();
        let daemon_released = released.clone();
        let daemon = tokio::spawn(async move {
            resume_rx.await.unwrap();
            let retained = work_path.is_dir();
            let _ = allocation_tx.send(AllocationLease(daemon_released));
            retained
        });
        let operation_released = released.clone();
        let caller = tokio::spawn(owned_startup(async move {
            let mut leases: Vec<_> = (0..prior_devices)
                .map(|_| AllocationLease(operation_released.clone()))
                .collect();
            entered_tx.send(()).unwrap();
            leases.push(allocation_rx.await.context("daemon allocation")?);
            registered_tx.send(()).unwrap();
            Ok((leases, work))
        }));
        entered_rx.await.unwrap();
        // Ensure the startup operation has reached the allocation await, including
        // any previously acquired root/extra devices.
        tokio::task::yield_now().await;
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        resume_tx.send(()).unwrap();
        let retained = daemon.await.unwrap();
        let registered = tokio::time::timeout(Duration::from_secs(2), registered_rx)
            .await
            .unwrap()
            .is_ok();
        tokio::task::yield_now().await;
        assert!(registered, "cancelled startup lost an in-flight allocation");
        assert!(
            retained,
            "work directory disappeared before allocation completed"
        );
        assert_eq!(
            released.load(std::sync::atomic::Ordering::SeqCst),
            prior_devices + 1
        );
    }

    #[tokio::test]
    async fn startup_cancel_during_root_allocation_keeps_cleanup_owned() {
        cancel_startup_during_allocation(0).await;
    }

    #[tokio::test]
    async fn startup_cancel_after_first_extra_keeps_cleanup_owned() {
        cancel_startup_during_allocation(2).await;
    }

    pub(super) struct ReleaseObserver {
        pid: u32,
        released: Option<tokio::sync::oneshot::Sender<bool>>,
    }

    impl Drop for ReleaseObserver {
        fn drop(&mut self) {
            if let Some(released) = self.released.take() {
                let _ = released.send(!PathBuf::from(format!("/proc/{}", self.pid)).exists());
            }
        }
    }

    #[tokio::test]
    async fn probe_diagnostic_io_failure_still_releases_artifacts() {
        let fixture = tempfile::tempdir().unwrap();
        let evidence_dir = fixture.path().join("not-a-directory");
        fs::write(&evidence_dir, b"occupied").unwrap();
        let mut runtime = RuntimeResources::new(tempfile::tempdir().unwrap(), None);
        let artifact = Arc::new(tempfile::tempdir().unwrap());
        let artifact_path = artifact.path().to_owned();
        runtime.retained_artifacts.push(artifact);
        let mut resources = ProbeResources {
            runtime,
            backing: None,
            evidence_dir,
        };
        resources.cleanup().await.unwrap();
        drop(resources);
        assert!(!artifact_path.exists());
    }

    #[tokio::test]
    async fn probe_cancel_keeps_cleanup_owned() {
        let child = tokio::process::Command::new("/bin/sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let (released, evidence) = tokio::sync::oneshot::channel();
        let evidence_work = tempfile::tempdir().unwrap();
        let mut runtime = RuntimeResources::new(tempfile::tempdir().unwrap(), None);
        runtime.process = Some(super::super::process::RunnerProcessGuard {
            child: child.into(),
            logs: None,
            fail_blocking_reap: false,
        });
        runtime.retained_artifacts.push(Arc::new(ReleaseObserver {
            pid,
            released: Some(released),
        }));
        let mut probe = DragonballM0Probe {
            resources: Some(ProbeResources {
                runtime,
                backing: None,
                evidence_dir: evidence_work.path().to_owned(),
            }),
            client: ProbeClient::new("/unused".into(), "cancel-test".into()),
            envd: None,
            address: String::new(),
            machine: None,
            config: None,
        };
        let mut stop = Box::pin(probe.stop());
        assert!(futures::poll!(stop.as_mut()).is_pending());
        drop(stop);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), evidence)
                .await
                .unwrap()
                .unwrap(),
            "a lease dropped before confirmed runner exit"
        );
    }

    #[test]
    fn unsupported_mode_fails_before_resource_allocation() {
        let mut allocations = 0;
        for (mode, arch, policy) in [
            (VirtualizationMode::Pvm, "x86_64", "host-default"),
            (VirtualizationMode::Kvm, "aarch64", "host-default"),
            (VirtualizationMode::Kvm, "x86_64", "intersection"),
        ] {
            let result = validate_profile(mode, arch, policy).map(|_| allocations += 1);
            assert!(result.is_err(), "unsupported profile allocated resources");
        }
        assert_eq!(allocations, 0);
    }
}
