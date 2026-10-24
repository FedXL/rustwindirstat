#[derive(Clone, Copy, Default)]
pub struct ProcessMemory {
    pub private_bytes: usize,
    pub peak_private_bytes: usize,
    pub peak_working_set_bytes: usize,
}

#[cfg(windows)]
pub fn process_memory() -> ProcessMemory {
    use std::ffi::c_void;
    #[repr(C)]
    #[derive(Default)]
    struct Counters {
        cb: u32,
        page_faults: u32,
        peak_working_set: usize,
        working_set: usize,
        peak_paged_pool: usize,
        paged_pool: usize,
        peak_nonpaged_pool: usize,
        nonpaged_pool: usize,
        pagefile: usize,
        peak_pagefile: usize,
        private: usize,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
    }
    #[link(name = "psapi")]
    unsafe extern "system" {
        fn GetProcessMemoryInfo(process: *mut c_void, counters: *mut Counters, size: u32) -> i32;
    }
    let mut counters = Counters {
        cb: std::mem::size_of::<Counters>() as u32,
        ..Counters::default()
    };
    // The pseudo-handle needs no close; the counter buffer has the advertised C layout.
    if unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            std::mem::size_of::<Counters>() as u32,
        )
    } == 0
    {
        return ProcessMemory::default();
    }
    ProcessMemory {
        private_bytes: counters.private,
        peak_private_bytes: counters.peak_pagefile,
        peak_working_set_bytes: counters.peak_working_set,
    }
}

#[cfg(not(windows))]
pub fn process_memory() -> ProcessMemory {
    ProcessMemory::default()
}
