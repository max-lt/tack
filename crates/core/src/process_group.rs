//! Process groups of the commands that ccrs starts: monitors and
//! terminals. A command runs as the leader of its own group, so a kill
//! reaches the processes it starts. The group is killed before the leader
//! is reaped: until then, no new process can take the group id.

use std::sync::Mutex;

/// The process groups of every monitor and terminal in this process.
pub(crate) static REGISTRY: Registry = Registry::new();

/// Kill the commands of every monitor and terminal in this process, and
/// refuse new ones after it. For a process that exits without unwinding,
/// such as on SIGTERM: these commands run in their own process groups, so
/// a signal to this process does not reach them.
pub fn kill_all() {
    REGISTRY.kill_all();
}

pub(crate) struct Registry {
    state: Mutex<RegistryState>,
    /// Tests: close the registry at the next `add`, as a kill_all that
    /// runs between the spawn and the add.
    #[cfg(test)]
    pub(crate) close_at_add: std::sync::atomic::AtomicBool,
}

struct RegistryState {
    groups: Vec<u32>,
    /// Set by `kill_all`; no group is added after it.
    closed: bool,
}

impl Registry {
    pub(crate) const fn new() -> Self {
        Self {
            state: Mutex::new(RegistryState {
                groups: Vec::new(),
                closed: false,
            }),
            #[cfg(test)]
            close_at_add: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed
    }

    /// Record a live group. After `kill_all`, kill it instead and return
    /// `false`.
    pub(crate) fn add(&self, pgid: u32) -> bool {
        let mut state = self.state.lock().unwrap();

        #[cfg(test)]
        if self.close_at_add.load(std::sync::atomic::Ordering::SeqCst) {
            state.closed = true;
        }

        if state.closed {
            killpg(pgid);
            return false;
        }

        state.groups.push(pgid);
        true
    }

    pub(crate) fn remove(&self, pgid: u32) {
        self.state.lock().unwrap().groups.retain(|&g| g != pgid);
    }

    pub(crate) fn kill_all(&self) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;

        for pgid in state.groups.drain(..) {
            killpg(pgid);
        }
    }
}

/// The process group of a command, until it is killed.
pub(crate) struct Group {
    pub(crate) pgid: Option<u32>,
    pub(crate) registry: &'static Registry,
}

impl Group {
    /// Kill every process in the group, once. Call it before the leader is
    /// reaped, or else the group id can belong to a new process.
    pub(crate) fn kill(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            self.registry.remove(pgid);
            killpg(pgid);
        }
    }
}

/// Wait until `pid` exits, without reaping it. The zombie keeps the pid
/// until `Child::wait`.
pub(crate) async fn exited(pid: u32) -> std::io::Result<()> {
    let wait = tokio::task::spawn_blocking(move || {
        let id = libc::id_t::from(pid);

        loop {
            // SAFETY: an all-zero siginfo_t is a valid value, and waitid
            // only writes into it.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: plain integers and a pointer to a local value.
            let done =
                unsafe { libc::waitid(libc::P_PID, id, &mut info, libc::WEXITED | libc::WNOWAIT) };

            if done == 0 {
                return Ok(());
            }

            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    });

    wait.await.unwrap_or_else(|e| Err(std::io::Error::other(e)))
}

pub(crate) fn killpg(pgid: u32) {
    if let Ok(pgid) = libc::pid_t::try_from(pgid) {
        // SAFETY: killpg takes plain integers. The group is ours: the
        // command was started as its leader.
        unsafe { libc::killpg(pgid, libc::SIGKILL) };
    }
}


/// Whether `pid` has exited, without reaping it.
pub(crate) fn has_exited(pid: u32) -> bool {
    // SAFETY: an all-zero siginfo_t is a valid value, and waitid only
    // writes into it.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: plain integers and a pointer to a local value.
    let done = unsafe {
        libc::waitid(
            libc::P_PID,
            libc::id_t::from(pid),
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };

    // SAFETY: waitid filled `info`; with WNOHANG, si_pid is 0 while the
    // process runs.
    done == 0 && unsafe { info.si_pid() } != 0
}
