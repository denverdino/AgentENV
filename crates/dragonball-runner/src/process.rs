use std::{
    fs,
    io::Write,
    os::{
        fd::AsRawFd,
        unix::{
            fs::{MetadataExt, PermissionsExt},
            net::UnixListener,
        },
    },
    path::{Path, PathBuf},
    time::Duration,
};

use crate::service::{ProbeVmm, RunnerService};
use agentenv_vmm_protocol::{encode_response, read_request, ProbePhase};
use anyhow::Context;

pub fn install_parent_death_guard(expected_parent: u32) -> anyhow::Result<()> {
    anyhow::ensure!(
        expected_parent > 1 && unsafe { libc::getppid() } as u32 == expected_parent,
        "runner parent changed before registration"
    );
    // PR_SET_PDEATHSIG belongs to the calling task: keep the node child main
    // thread alive for the full runner lifetime, including before VMM setup.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error()).context("register runner parent death signal");
    }
    anyhow::ensure!(
        unsafe { libc::getppid() } as u32 == expected_parent,
        "runner parent exited during registration"
    );
    Ok(())
}

pub struct PrivateListener {
    listener: UnixListener,
    socket_path: PathBuf,
}

impl PrivateListener {
    pub fn bind(socket: &Path, work_dir: &Path) -> anyhow::Result<Self> {
        anyhow::ensure!(
            socket.is_absolute() && work_dir.is_absolute(),
            "runner paths must be absolute"
        );
        let work_dir = fs::canonicalize(work_dir).context("resolve runner work directory")?;
        let meta = fs::symlink_metadata(&work_dir)?;
        anyhow::ensure!(
            meta.is_dir()
                && meta.uid() == unsafe { libc::geteuid() }
                && meta.permissions().mode() & 0o777 == 0o700,
            "work directory must be owned by runner and mode 0700"
        );
        anyhow::ensure!(
            fs::canonicalize(socket.parent().context("socket has no parent")?)? == work_dir,
            "socket must be directly inside work directory"
        );
        let listener = UnixListener::bind(socket).context("bind private VMM socket")?;
        let private = Self {
            listener,
            socket_path: socket.into(),
        };
        fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
        Ok(private)
    }

    pub fn serve<V: ProbeVmm>(&self, service: &mut RunnerService<V>) -> anyhow::Result<()> {
        loop {
            let (mut stream, _) = match self.listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error).context("accept private VMM request"),
            };
            if peer_uid(stream.as_raw_fd())? != unsafe { libc::geteuid() } {
                continue;
            }
            // One frame per connection; a broken/idle peer cannot monopolize
            // the serial API forever. The node applies its overall deadline too.
            stream.set_read_timeout(Some(Duration::from_secs(1)))?;
            stream.set_write_timeout(Some(Duration::from_secs(1)))?;
            let request = match read_request(&mut stream) {
                Ok(request) => request,
                Err(_) => continue,
            };
            let response = service.handle(request);
            let write_result = stream.write_all(&encode_response(&response)?);
            match service.phase() {
                ProbePhase::Stopped => {
                    write_result?;
                    return Ok(());
                }
                ProbePhase::Failed => {
                    return Err(anyhow::anyhow!(
                        "VMM operation failed; runner requires teardown"
                    ))
                }
                _ => {
                    let _ = write_result;
                }
            }
        }
    }

    pub fn serve_runtime<V: crate::runtime_service::RuntimeVmm>(
        &self,
        service: &mut crate::runtime_service::RuntimeService<V>,
    ) -> anyhow::Result<()> {
        use agentenv_vmm_protocol::{
            encode_runtime_response, read_runtime_request, runtime::RuntimePhase,
        };
        loop {
            let (mut stream, _) = match self.listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error).context("accept runtime request"),
            };
            if peer_uid(stream.as_raw_fd())? != unsafe { libc::geteuid() } {
                continue;
            }
            stream.set_read_timeout(Some(Duration::from_secs(1)))?;
            stream.set_write_timeout(Some(Duration::from_secs(1)))?;
            let request = match read_runtime_request(&mut RuntimeFrameReader {
                stream: &mut stream,
                deadline: std::time::Instant::now() + Duration::from_secs(1),
            }) {
                Ok(request) => request,
                Err(_) => continue,
            };
            let response = service.handle(request);
            let written = stream.write_all(&encode_runtime_response(&response)?);
            match service.phase() {
                RuntimePhase::Stopped => {
                    written?;
                    return Ok(());
                }
                RuntimePhase::Failed => {
                    return Err(anyhow::anyhow!(
                        "runtime outcome unconfirmed; runner requires teardown"
                    ))
                }
                _ => {
                    let _ = written;
                }
            }
        }
    }
}

struct RuntimeFrameReader<'a> {
    stream: &'a mut std::os::unix::net::UnixStream,
    deadline: std::time::Instant,
}

impl std::io::Read for RuntimeFrameReader<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self
            .deadline
            .saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "runtime frame read deadline exceeded",
            ));
        }
        self.stream.set_read_timeout(Some(remaining))?;
        std::io::Read::read(self.stream, bytes)
    }
}

fn peer_uid(fd: std::os::fd::RawFd) -> anyhow::Result<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut length,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error()).context("read VMM peer credentials");
    }
    anyhow::ensure!(
        length as usize == std::mem::size_of::<libc::ucred>(),
        "invalid peer credentials"
    );
    Ok(cred.uid)
}

impl Drop for PrivateListener {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_accept_waits_for_a_connection() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let socket = directory.path().join("control.sock");
        let listener = PrivateListener::bind(&socket, directory.path()).unwrap();
        let (accepted_tx, accepted_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            accepted_tx
                .send(listener.listener.accept().map(|_| ()))
                .unwrap();
        });
        let idle = accepted_rx.recv_timeout(Duration::from_millis(50));
        assert!(matches!(
            idle,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        let _connection = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        accepted_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        worker.join().unwrap();
    }
}
