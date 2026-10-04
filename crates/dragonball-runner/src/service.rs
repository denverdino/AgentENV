use std::{collections::HashSet, time::Duration};

use agentenv_vmm_protocol::{
    validate_request, ProbeCommand, ProbeError, ProbePhase, ProbeReply, ProbeVmConfig,
    RequestEnvelope, ResponseEnvelope, RunnerIdentity, PROTOCOL_VERSION,
};

pub trait ProbeVmm {
    fn set_budget(&mut self, _budget: Duration) {}
    fn configure(&mut self, config: &ProbeVmConfig) -> anyhow::Result<()>;
    fn start(&mut self) -> anyhow::Result<()>;
    fn pause(&mut self) -> anyhow::Result<()>;
    fn resume(&mut self) -> anyhow::Result<()>;
    fn stop(&mut self) -> anyhow::Result<()>;
    fn save_quiesced(
        &mut self,
        _state: &std::path::Path,
        _memory: &std::path::Path,
    ) -> anyhow::Result<()> {
        anyhow::bail!("snapshot probe not implemented")
    }
    fn restore_quiesced(
        &mut self,
        _state: &std::path::Path,
        _memory: &std::path::Path,
    ) -> anyhow::Result<()> {
        anyhow::bail!("snapshot probe not implemented")
    }
}

pub struct RunnerService<V: ProbeVmm> {
    pub vmm: V,
    instance_id: String,
    phase: ProbePhase,
    last_request_id: Option<u64>,
}

impl<V: ProbeVmm> RunnerService<V> {
    pub fn new(instance_id: String, vmm: V) -> Self {
        Self {
            vmm,
            instance_id,
            phase: ProbePhase::Unconfigured,
            last_request_id: None,
        }
    }

    pub fn handle(&mut self, request: RequestEnvelope) -> ResponseEnvelope {
        let result = if let Err(error) = validate_request(&request) {
            Err(probe_error("invalid_request", error.to_string()))
        } else if request.sandbox_instance_id != self.instance_id {
            Err(probe_error(
                "wrong_instance",
                "request belongs to another instance",
            ))
        } else if self
            .last_request_id
            .is_some_and(|id| request.request_id <= id)
        {
            Err(probe_error(
                "replayed_request",
                "request IDs must strictly increase",
            ))
        } else {
            self.last_request_id = Some(request.request_id);
            self.vmm
                .set_budget(Duration::from_millis(request.budget_ms));
            self.dispatch(request.command)
        };
        ResponseEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: request.request_id,
            sandbox_instance_id: self.instance_id.clone(),
            result,
        }
    }

    pub fn phase(&self) -> ProbePhase {
        self.phase
    }

    fn dispatch(&mut self, command: ProbeCommand) -> Result<ProbeReply, ProbeError> {
        use ProbeCommand::*;
        let (action, phase) = match command {
            Hello => {
                let identity = crate::build_identity();
                return Ok(ProbeReply::Hello(RunnerIdentity {
                    protocol_version: identity.protocol_version,
                    runner_version: identity.runner_version,
                    dragonball_revision: identity.dragonball_revision,
                    feature_profile: identity.feature_profile,
                }));
            }
            Status => return Ok(ProbeReply::Status(self.phase)),
            SaveQuiesced {
                state_path,
                memory_path,
            } if self.phase == ProbePhase::Paused => (
                self.vmm.save_quiesced(&state_path, &memory_path),
                ProbePhase::Paused,
            ),
            RestoreQuiesced {
                state_path,
                memory_path,
            } if self.phase == ProbePhase::Configured => (
                self.vmm.restore_quiesced(&state_path, &memory_path),
                ProbePhase::Running,
            ),
            Configure(config) if self.phase == ProbePhase::Unconfigured => {
                validate_config(&config)
                    .map_err(|e| probe_error("invalid_config", e.to_string()))?;
                (self.vmm.configure(&config), ProbePhase::Configured)
            }
            Start if self.phase == ProbePhase::Configured => {
                (self.vmm.start(), ProbePhase::Running)
            }
            Pause if self.phase == ProbePhase::Running => (self.vmm.pause(), ProbePhase::Paused),
            Resume if self.phase == ProbePhase::Paused => (self.vmm.resume(), ProbePhase::Running),
            Stop if self.phase == ProbePhase::Stopped => return Ok(ProbeReply::Done),
            Stop => (self.vmm.stop(), ProbePhase::Stopped),
            _ => {
                return Err(probe_error(
                    "invalid_phase",
                    format!("command not allowed in {:?}", self.phase),
                ))
            }
        };
        match action {
            Ok(()) => {
                self.phase = phase;
                Ok(ProbeReply::Done)
            }
            Err(error) if error.is::<crate::probe_snapshot::SnapshotPreflightError>() => {
                Err(probe_error("snapshot_preflight_error", error.to_string()))
            }
            Err(error) => {
                self.phase = ProbePhase::Failed;
                // No recovery is advertised in M0. Tear down even a partial boot.
                let cleanup = if phase != ProbePhase::Stopped {
                    self.vmm.stop()
                } else {
                    Ok(())
                };
                let message = match cleanup {
                    Ok(()) => error.to_string(),
                    Err(cleanup) => format!("{error}; cleanup failed: {cleanup}"),
                };
                Err(probe_error("vmm_error", message))
            }
        }
    }
}

pub fn validate_config(config: &ProbeVmConfig) -> anyhow::Result<()> {
    anyhow::ensure!(
        config.vcpu_count > 0 && config.memory_mib > 0,
        "CPU and memory must be nonzero"
    );
    anyhow::ensure!(
        config.kernel_path.is_absolute(),
        "kernel path must be absolute"
    );
    anyhow::ensure!(
        !config.block_devices.is_empty(),
        "at least one block device is required"
    );
    let mut ids = HashSet::new();
    for block in &config.block_devices {
        anyhow::ensure!(
            !block.drive_id.is_empty() && ids.insert(&block.drive_id),
            "empty or duplicate device ID"
        );
        anyhow::ensure!(
            block.path_on_host.is_absolute(),
            "block path must be absolute"
        );
    }
    anyhow::ensure!(
        config.block_devices.iter().filter(|d| d.is_root).count() == 1,
        "exactly one root drive is required"
    );
    anyhow::ensure!(
        !config.network.iface_id.is_empty() && ids.insert(&config.network.iface_id),
        "empty or duplicate network ID"
    );
    anyhow::ensure!(
        !config.network.tap_name.is_empty()
            && config.network.tap_name.len() < 16
            && !config.network.tap_name.as_bytes().contains(&0),
        "invalid TAP name"
    );
    Ok(())
}

fn probe_error(code: &str, message: impl Into<String>) -> ProbeError {
    ProbeError {
        code: code.into(),
        message: message.into(),
    }
}
