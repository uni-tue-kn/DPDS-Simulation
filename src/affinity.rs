//! CPU pinning for the sweep's worker threads (`--pin-workers`).

/// The CPU worker `worker` is pinned to: the `(worker mod n)`-th of the
/// `n` allowed CPUs, in the order given (ascending as read by
/// [`allowed_cpus`]).
///
/// # Panics
///
/// If `allowed` is empty.
pub fn worker_cpu(allowed: &[usize], worker: usize) -> usize {
    allowed[worker % allowed.len()]
}

/// The CPUs the process may run on, ascending (`sched_getaffinity`).
#[cfg(target_os = "linux")]
pub fn allowed_cpus() -> Result<Vec<usize>, String> {
    // SAFETY: an all-zero `cpu_set_t` is a valid (empty) set, the kernel
    // writes at most `size_of::<cpu_set_t>()` bytes into it, and
    // `CPU_ISSET` only reads within the set for CPUs below `CPU_SETSIZE`.
    let cpus: Vec<usize> = unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut set) != 0 {
            return Err(format!(
                "sched_getaffinity failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        (0..libc::CPU_SETSIZE as usize)
            .filter(|&cpu| libc::CPU_ISSET(cpu, &set))
            .collect()
    };
    if cpus.is_empty() {
        return Err("the process is allowed to run on no CPU".to_string());
    }
    Ok(cpus)
}

/// Pins the calling thread to `cpu` (`sched_setaffinity`).
#[cfg(target_os = "linux")]
pub fn pin_current_thread(cpu: usize) -> Result<(), String> {
    // SAFETY: as in `allowed_cpus`; pid 0 means the calling thread.
    let result = unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set)
    };
    if result != 0 {
        return Err(format!(
            "cannot pin a thread to CPU {cpu}: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn allowed_cpus() -> Result<Vec<usize>, String> {
    Err("--pin-workers is unsupported on this platform (Linux only)".to_string())
}

#[cfg(not(target_os = "linux"))]
pub fn pin_current_thread(_cpu: usize) -> Result<(), String> {
    Err("--pin-workers is unsupported on this platform (Linux only)".to_string())
}
