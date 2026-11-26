/// Per-thread CPU time used by the codec counters on the supported platforms.
#[cfg(windows)]
pub fn thread_cpu_ns() -> u64 {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThread() -> *mut std::ffi::c_void;
        fn GetThreadTimes(
            t: *mut std::ffi::c_void,
            create: *mut u64,
            exit: *mut u64,
            kernel: *mut u64,
            user: *mut u64,
        ) -> i32;
    }
    let (mut create, mut exit, mut kernel, mut user) = (0, 0, 0, 0);
    unsafe {
        if GetThreadTimes(
            GetCurrentThread(),
            &mut create,
            &mut exit,
            &mut kernel,
            &mut user,
        ) == 0
        {
            return 0;
        }
    }
    (kernel + user) * 100
}
#[cfg(target_os = "linux")]
pub fn thread_cpu_ns() -> u64 {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        if libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut t) != 0 {
            return 0;
        }
    }
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}
#[cfg(not(any(windows, target_os = "linux")))]
pub fn thread_cpu_ns() -> u64 {
    0
}

#[cfg(windows)]
pub fn thread_cpu_cycles() -> u64 {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThread() -> *mut std::ffi::c_void;
        fn QueryThreadCycleTime(t: *mut std::ffi::c_void, cycles: *mut u64) -> i32;
    }
    let mut cycles = 0;
    unsafe {
        QueryThreadCycleTime(GetCurrentThread(), &mut cycles);
    }
    cycles
}
#[cfg(not(windows))]
pub fn thread_cpu_cycles() -> u64 {
    0
}

/// Cumulative CPU time across every process thread and current resident bytes.
/// Sample at control-plane frequency, outside per-entity loops.
#[cfg(windows)]
pub fn process_usage() -> (u64, u64) {
    #[repr(C)]
    #[derive(Default)]
    struct Memory {
        size: u32,
        faults: u32,
        peak: usize,
        resident: usize,
        pools_and_pagefile: [usize; 6],
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> *mut std::ffi::c_void;
        fn GetProcessTimes(
            process: *mut std::ffi::c_void,
            create: *mut u64,
            exit: *mut u64,
            kernel: *mut u64,
            user: *mut u64,
        ) -> i32;
        fn K32GetProcessMemoryInfo(
            process: *mut std::ffi::c_void,
            memory: *mut Memory,
            size: u32,
        ) -> i32;
    }
    let mut memory = Memory {
        size: std::mem::size_of::<Memory>() as u32,
        ..Default::default()
    };
    let (mut create, mut exit, mut kernel, mut user) = (0, 0, 0, 0);
    unsafe {
        let p = GetCurrentProcess();
        GetProcessTimes(p, &mut create, &mut exit, &mut kernel, &mut user);
        K32GetProcessMemoryInfo(p, &mut memory, memory.size);
    }
    ((kernel + user) * 100, memory.resident as u64)
}
#[cfg(target_os = "linux")]
pub fn process_usage() -> (u64, u64) {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let rss = std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
        .unwrap_or(0);
    unsafe {
        libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut t);
        (
            t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64,
            rss.saturating_mul(libc::sysconf(libc::_SC_PAGESIZE).max(0) as u64),
        )
    }
}
#[cfg(not(any(windows, target_os = "linux")))]
pub fn process_usage() -> (u64, u64) {
    (0, 0)
}
