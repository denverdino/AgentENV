use agentenv_vmm_protocol::runtime_memory::MemoryMapPager;
use agentenv_vmm_protocol::{runtime::*, validate_runtime_request};
use std::{path::Path, time::Duration};

pub const RUNTIME_PROFILE: &str = RUNTIME_FEATURE_PROFILE;
pub const RESTORE_ABI: &str = RESTORE_ABI_ID;

/// The adapter has positively confirmed a usable VM state, not merely timed out.
/// Outputs from a failed export can still be partial and must never be published.
#[derive(Debug)]
pub struct RuntimeKnownError(pub anyhow::Error);
impl std::fmt::Display for RuntimeKnownError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for RuntimeKnownError {}

pub fn restore_identity(
    kernel_version: String,
    tools_drive_version: String,
    envd_version: String,
) -> SnapshotRestoreIdentity {
    SnapshotRestoreIdentity {
        backend: "dragonball".into(),
        architecture: "x86_64".into(),
        virtualization_mode: "kvm".into(),
        runner_build_id: env!("RUNTIME_BUILD_ID").into(),
        dragonball_revision: env!("DRAGONBALL_REVISION").into(),
        snapshot_format_epoch: SNAPSHOT_FORMAT_EPOCH,
        restore_abi_id: RESTORE_ABI.into(),
        cpu_policy_id: "host-default".into(),
        device_profile_id: RUNTIME_FEATURE_PROFILE.into(),
        memory_layout_id: "packed-ram-4k-v2".into(),
        kernel_version,
        tools_drive_version,
        envd_version,
    }
}

pub trait RuntimeVmm {
    fn set_budget(&mut self, _budget: Duration) {}
    fn configure(&mut self, config: &RuntimeVmConfig) -> anyhow::Result<()>;
    fn start(&mut self) -> anyhow::Result<()>;
    fn begin_capture(&mut self, generation: u64) -> anyhow::Result<CaptureReport>;
    fn export_held(&mut self, generation: u64, state: &Path, memory: &Path) -> anyhow::Result<()>;
    fn export_held_state(&mut self, _generation: u64, _state: &Path) -> anyhow::Result<()> {
        anyhow::bail!("state-only export is unavailable")
    }
    fn export_memory_map(
        &mut self,
        _generation: u64,
    ) -> anyhow::Result<agentenv_vmm_protocol::runtime_memory::FrozenMemoryMap> {
        anyhow::bail!("memory map export is unavailable")
    }
    fn release_memory_map(&mut self, _generation: u64) -> anyhow::Result<()> {
        anyhow::bail!("memory map release is unavailable")
    }
    fn end_capture(&mut self, generation: u64) -> anyhow::Result<()>;
    fn load_held(
        &mut self,
        generation: u64,
        input: &RuntimeSnapshotInput,
    ) -> anyhow::Result<CaptureReport>;
    fn stop(&mut self) -> anyhow::Result<()>;
}

pub struct RuntimeService<V: RuntimeVmm> {
    pub vmm: V,
    instance_id: String,
    runner_instance_id: String,
    phase: RuntimePhase,
    last_request_id: Option<u64>,
    generation: u64,
    capture: Option<CaptureReport>,
    machine: Option<RuntimeVmConfig>,
    memory_map: Option<(MemoryMapPager, Option<u64>)>,
    // Also true for an unknown export outcome until teardown is confirmed.
    memory_map_pinned: bool,
}

impl<V: RuntimeVmm> RuntimeService<V> {
    pub fn new(instance_id: String, runner_instance_id: String, vmm: V) -> Self {
        Self {
            vmm,
            instance_id,
            runner_instance_id,
            phase: RuntimePhase::Unconfigured,
            last_request_id: None,
            generation: 0,
            capture: None,
            machine: None,
            memory_map: None,
            memory_map_pinned: false,
        }
    }

    pub fn phase(&self) -> RuntimePhase {
        self.phase
    }

    pub fn handle(&mut self, request: RuntimeRequest) -> RuntimeResponse {
        let result = if let Err(error) = validate_runtime_request(&request) {
            Err(runtime_error("invalid_request", error.to_string()))
        } else if request.sandbox_instance_id != self.instance_id {
            Err(runtime_error(
                "wrong_instance",
                "request belongs to another sandbox instance",
            ))
        } else if self
            .last_request_id
            .is_some_and(|id| request.request_id <= id)
        {
            Err(runtime_error(
                "replayed_request",
                "request IDs must strictly increase",
            ))
        } else {
            self.last_request_id = Some(request.request_id);
            self.vmm
                .set_budget(Duration::from_millis(request.budget_ms));
            self.dispatch(request.command)
        };
        RuntimeResponse {
            mode: "runtime".into(),
            protocol_version: RUNTIME_PROTOCOL_VERSION,
            request_id: request.request_id,
            sandbox_instance_id: self.instance_id.clone(),
            result,
        }
    }

    fn dispatch(&mut self, command: RuntimeCommand) -> Result<RuntimeReply, RuntimeError> {
        let outcome = match command {
            RuntimeCommand::Hello => {
                let identity = restore_identity(String::new(), String::new(), String::new());
                return Ok(RuntimeReply::Hello(RuntimeHello {
                    runner_instance_id: self.runner_instance_id.clone(),
                    runner_build_id: identity.runner_build_id,
                    dragonball_revision: identity.dragonball_revision,
                    feature_profile: RUNTIME_PROFILE.into(),
                    snapshot_format_epoch: identity.snapshot_format_epoch,
                    restore_abi_id: identity.restore_abi_id,
                    capabilities: vec![
                        "full_memory_capture".into(),
                        "held_restore".into(),
                        "state_only_capture".into(),
                        "leased_memory_export".into(),
                    ],
                }));
            }
            RuntimeCommand::Status => {
                return Ok(RuntimeReply::Status(RuntimeStatus {
                    phase: self.phase,
                    capture: self.capture.clone(),
                    memory_map_pinned: self.memory_map_pinned,
                }))
            }
            RuntimeCommand::Configure(config) if self.phase == RuntimePhase::Unconfigured => {
                validate_runtime_config(&config)
                    .map_err(|e| runtime_error("invalid_config", e.to_string()))?;
                self.vmm.configure(&config).map(|()| {
                    self.machine = Some(config);
                    self.phase = RuntimePhase::Configured;
                    RuntimeReply::Done
                })
            }
            RuntimeCommand::Start if self.phase == RuntimePhase::Configured => {
                self.vmm.start().map(|()| {
                    self.phase = RuntimePhase::Running;
                    RuntimeReply::Done
                })
            }
            RuntimeCommand::BeginCapture if self.phase == RuntimePhase::Running => {
                self.next_generation()?;
                self.phase = RuntimePhase::Capturing;
                match self.vmm.begin_capture(self.generation) {
                    Ok(report) => self.keep_report(report, RuntimePhase::Held),
                    Err(error) if error.is::<RuntimeKnownError>() => {
                        self.phase = RuntimePhase::Running;
                        Err(error)
                    }
                    Err(error) => Err(error),
                }
            }
            RuntimeCommand::ExportHeldSnapshot {
                token,
                state_path,
                memory_path,
            } if matches!(self.phase, RuntimePhase::Held | RuntimePhase::LoadedHeld) => {
                self.check_token(&token)?;
                self.vmm
                    .export_held(token.generation, &state_path, &memory_path)
                    .map(|()| RuntimeReply::Done)
            }
            RuntimeCommand::EndCapture { token } if self.phase == RuntimePhase::Held => {
                self.release(&token)?
            }
            RuntimeCommand::ExportHeldState { token, state_path }
                if matches!(self.phase, RuntimePhase::Held | RuntimePhase::LoadedHeld) =>
            {
                self.check_token(&token)?;
                self.vmm
                    .export_held_state(token.generation, &state_path)
                    .map(|()| RuntimeReply::Done)
            }
            RuntimeCommand::ExportMemoryMap { token, cursor }
                if matches!(self.phase, RuntimePhase::Held | RuntimePhase::LoadedHeld) =>
            {
                self.check_token(&token)?;
                let expected = self.memory_map.as_ref().map_or(Some(0), |(_, next)| *next);
                if expected != Some(cursor) {
                    return Err(runtime_error(
                        "invalid_map_cursor",
                        "memory pages must be read once, in order",
                    ));
                }
                (|| -> anyhow::Result<RuntimeReply> {
                    if self.memory_map.is_none() {
                        self.memory_map_pinned = true;
                        let mut map = match self.vmm.export_memory_map(token.generation) {
                            Ok(map) => map,
                            Err(error) => {
                                if error.is::<RuntimeKnownError>() {
                                    self.memory_map_pinned = false;
                                }
                                return Err(error);
                            }
                        };
                        map.token.runner_instance_id = self.runner_instance_id.clone();
                        anyhow::ensure!(
                            map.token == token
                                && map.layout_id
                                    == self.machine.as_ref().unwrap().identity.memory_layout_id
                                && map.validate().map_err(anyhow::Error::msg)?
                                    == self.capture.as_ref().unwrap().memory_layout,
                            "memory export does not match held proof"
                        );
                        self.memory_map = Some((
                            MemoryMapPager::new(map).map_err(anyhow::Error::msg)?,
                            Some(0),
                        ));
                    }
                    let (pager, next) = self.memory_map.as_mut().unwrap();
                    let chunk = pager.page(cursor).map_err(anyhow::Error::msg)?;
                    *next = chunk.next_cursor;
                    Ok(RuntimeReply::MemoryMap(chunk))
                })()
            }
            RuntimeCommand::ReleaseMemoryMap { token }
                if matches!(self.phase, RuntimePhase::Held | RuntimePhase::LoadedHeld) =>
            {
                self.check_token(&token)?;
                if !self.memory_map_pinned {
                    return Ok(RuntimeReply::Done);
                }
                self.vmm.release_memory_map(token.generation).map(|()| {
                    self.memory_map = None;
                    self.memory_map_pinned = false;
                    RuntimeReply::Done
                })
            }
            RuntimeCommand::Resume { token } if self.phase == RuntimePhase::LoadedHeld => {
                self.release(&token)?
            }
            RuntimeCommand::LoadHeld(input) if self.phase == RuntimePhase::Configured => {
                let config = self.machine.as_ref().expect("configured machine");
                if input.probe_only || !input.identity.is_restore_compatible_with(&config.identity)
                {
                    return Err(runtime_error(
                        "snapshot_preflight_error",
                        "probe or incompatible snapshot",
                    ));
                }
                self.next_generation()?;
                match self.vmm.load_held(self.generation, &input) {
                    Ok(report) => self.keep_report(report, RuntimePhase::LoadedHeld),
                    Err(error) => Err(error),
                }
            }
            RuntimeCommand::Stop if self.phase == RuntimePhase::Stopped => {
                return Ok(RuntimeReply::Done)
            }
            RuntimeCommand::Stop => self.vmm.stop().map(|()| {
                self.capture = None;
                self.memory_map = None;
                self.memory_map_pinned = false;
                self.phase = RuntimePhase::Stopped;
                RuntimeReply::Done
            }),
            _ => {
                return Err(runtime_error(
                    "invalid_phase",
                    format!("command not allowed in {:?}", self.phase),
                ))
            }
        };
        match outcome {
            Ok(reply) => Ok(reply),
            Err(error) if error.is::<RuntimeKnownError>() => {
                Err(runtime_error("snapshot_preflight_error", error.to_string()))
            }
            Err(error) => {
                self.phase = RuntimePhase::Failed;
                self.capture = None;
                // A timeout is an unknown outcome, not cancellation. No action is reissued.
                let cleanup = self.vmm.stop();
                let message = match cleanup {
                    Ok(()) => {
                        self.memory_map = None;
                        self.memory_map_pinned = false;
                        error.to_string()
                    }
                    Err(cleanup) => format!("{error}; teardown failed: {cleanup}"),
                };
                Err(runtime_error("vmm_error", message))
            }
        }
    }

    fn next_generation(&mut self) -> Result<(), RuntimeError> {
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| runtime_error("generation_exhausted", "capture generation exhausted"))?;
        Ok(())
    }

    fn check_token(&self, token: &CaptureToken) -> Result<(), RuntimeError> {
        if self.capture.as_ref().map(|report| &report.token) != Some(token) || token.generation == 0
        {
            return Err(runtime_error(
                "stale_capture_token",
                "capture token does not own this hold",
            ));
        }
        Ok(())
    }

    fn release(
        &mut self,
        token: &CaptureToken,
    ) -> Result<anyhow::Result<RuntimeReply>, RuntimeError> {
        self.check_token(token)?;
        if self.memory_map_pinned {
            return Err(runtime_error(
                "memory_map_pinned",
                "drain readers and release the memory map before resuming",
            ));
        }
        Ok(self.vmm.end_capture(token.generation).map(|()| {
            self.capture = None;
            self.phase = RuntimePhase::Running;
            RuntimeReply::Done
        }))
    }

    fn keep_report(
        &mut self,
        mut report: CaptureReport,
        phase: RuntimePhase,
    ) -> anyhow::Result<RuntimeReply> {
        report.token.runner_instance_id = self.runner_instance_id.clone();
        report.validate().map_err(anyhow::Error::msg)?;
        validate_capture_report(
            self.machine.as_ref().expect("configured machine"),
            self.generation,
            &report,
        )?;
        self.capture = Some(report.clone());
        self.phase = phase;
        Ok(RuntimeReply::Held(report))
    }
}

pub(crate) fn probe_config(config: &RuntimeVmConfig) -> agentenv_vmm_protocol::ProbeVmConfig {
    use agentenv_vmm_protocol::{ProbeBlockDevice, ProbeNetworkDevice, ProbeVmConfig};
    ProbeVmConfig {
        vcpu_count: config.vcpu_count,
        memory_mib: config.memory_mib,
        kernel_path: config.kernel_path.clone(),
        boot_args: config.boot_args.clone(),
        block_devices: config
            .block_devices
            .iter()
            .map(|disk| ProbeBlockDevice {
                drive_id: disk.drive_id.clone(),
                path_on_host: disk.path_on_host.clone(),
                read_only: disk.read_only,
                is_root: disk.is_root,
            })
            .collect(),
        network: ProbeNetworkDevice {
            iface_id: config.network.iface_id.clone(),
            tap_name: config.network.tap_name.clone(),
        },
    }
}

pub fn validate_runtime_config(config: &RuntimeVmConfig) -> anyhow::Result<()> {
    crate::service::validate_config(&probe_config(config))?;
    for disk in &config.block_devices {
        anyhow::ensure!(
            if disk.is_volume_slot {
                !disk.is_root && disk.backing_read_only.is_some() != disk.read_only
            } else {
                disk.backing_read_only.is_none()
            },
            "invalid reserved slot access policy"
        );
        if let Some(rate) = &disk.rate_limiter {
            rate.validate().map_err(anyhow::Error::msg)?;
        }
    }
    anyhow::ensure!(
        config.block_devices.len() < 256,
        "configured worker set exceeds protocol limit"
    );
    anyhow::ensure!(
        !config.identity.kernel_version.is_empty()
            && !config.identity.tools_drive_version.is_empty()
            && !config.identity.envd_version.is_empty(),
        "guest provenance versions are required"
    );
    let expected = restore_identity(
        config.identity.kernel_version.clone(),
        config.identity.tools_drive_version.clone(),
        config.identity.envd_version.clone(),
    );
    anyhow::ensure!(
        config.identity == expected && config.identity.architecture == "x86_64",
        "unsupported runtime restore identity"
    );
    Ok(())
}

fn validate_capture_report(
    config: &RuntimeVmConfig,
    generation: u64,
    report: &CaptureReport,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        report.token.generation == generation,
        "capture generation mismatch"
    );
    report
        .memory_layout
        .validate()
        .map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        report.memory_layout.image_size == u64::from(config.memory_mib) * 1024 * 1024,
        "capture RAM size differs from configured machine"
    );
    anyhow::ensure!(
        report.vcpu_ids == (0..config.vcpu_count).collect::<Vec<_>>(),
        "incomplete vCPU acknowledgement set"
    );
    let expected = std::iter::once("balloon:memory".to_owned())
        .chain(
            config
                .block_devices
                .iter()
                .filter(|disk| disk.is_root)
                .chain(config.block_devices.iter().filter(|disk| !disk.is_root))
                .map(|disk| format!("block:{}/q0", disk.drive_id))
                .chain(std::iter::once(format!("net:{}", config.network.iface_id))),
        )
        .collect::<Vec<_>>();
    anyhow::ensure!(
        report.worker_acks.len() == expected.len(),
        "incomplete worker acknowledgement set"
    );
    for (ack, id) in report.worker_acks.iter().zip(expected) {
        anyhow::ensure!(
            ack.device_id == id
                && ack.generation == generation
                && ack.pending_io == 0
                && ack.memory_writers == 0
                && (!id.starts_with("block:") || ack.flush_completed),
            "invalid worker acknowledgement"
        );
    }
    Ok(())
}

fn runtime_error(code: &str, message: impl Into<String>) -> RuntimeError {
    RuntimeError {
        code: code.into(),
        message: message.into(),
    }
}
