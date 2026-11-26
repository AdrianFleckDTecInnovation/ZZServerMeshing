//! Apply a CPU budget before starting Bevy, network or checkpoint threads.
use crate::{invalid, Result};
use std::num::NonZeroUsize;

/// Restrict this process (the initial thread and all subsequently created threads on Linux)
/// to eligible logical CPUs. Explicit IDs select a topology; otherwise preserve OS numbering.
/// On Windows the IDs belong to one processor group. Use one OS-pinned process per group.
pub fn apply_cpu_budget(cores: NonZeroUsize, requested: Option<&[usize]>) -> Result<Vec<usize>> {
    let eligible = allowed()?;
    let selected = requested.map_or_else(
        || eligible.iter().copied().take(cores.get()).collect(),
        |ids| ids.to_vec(),
    );
    if selected.len() != cores.get()
        || selected.iter().any(|cpu| !eligible.contains(cpu))
        || selected
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != selected.len()
    {
        return Err(invalid(
            "CPU budget must select distinct eligible logical CPUs",
        ));
    }
    if selected != eligible {
        bind(&selected)?;
    }
    Ok(selected)
}
#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcess() -> *mut std::ffi::c_void;
    fn GetProcessAffinityMask(
        p: *mut std::ffi::c_void,
        process: *mut usize,
        system: *mut usize,
    ) -> i32;
    fn SetProcessAffinityMask(p: *mut std::ffi::c_void, mask: usize) -> i32;
}
#[cfg(windows)]
fn allowed() -> Result<Vec<usize>> {
    let (mut p, mut s) = (0usize, 0usize);
    if unsafe { GetProcessAffinityMask(GetCurrentProcess(), &mut p, &mut s) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if p == 0 {
        return Err(invalid("assign this worker process to one Windows processor group before applying CPU affinity"));
    }
    Ok((0..usize::BITS as usize)
        .filter(|i| p & (1usize << i) != 0)
        .collect())
}
#[cfg(windows)]
fn bind(ids: &[usize]) -> Result<()> {
    let mask = ids.iter().fold(0usize, |m, &i| m | (1usize << i));
    if unsafe { SetProcessAffinityMask(GetCurrentProcess(), mask) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
#[cfg(target_os = "linux")]
fn allowed() -> Result<Vec<usize>> {
    let mut mask: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    if unsafe { libc::sched_getaffinity(0, std::mem::size_of_val(&mask), &mut mask) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok((0..libc::CPU_SETSIZE as usize)
        .filter(|&i| unsafe { libc::CPU_ISSET(i, &mask) })
        .collect())
}
#[cfg(target_os = "linux")]
fn bind(ids: &[usize]) -> Result<()> {
    let mut mask: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    for &i in ids {
        unsafe { libc::CPU_SET(i, &mut mask) };
    }
    if unsafe { libc::sched_setaffinity(0, std::mem::size_of_val(&mask), &mask) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
#[cfg(not(any(target_os = "linux", windows)))]
fn allowed() -> Result<Vec<usize>> {
    Err(invalid("CPU affinity requires Linux or Windows"))
}
#[cfg(not(any(target_os = "linux", windows)))]
fn bind(_: &[usize]) -> Result<()> {
    Err(invalid("CPU affinity requires Linux or Windows"))
}
