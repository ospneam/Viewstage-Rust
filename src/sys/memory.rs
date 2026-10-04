/// 清理系统工作集（Windows）
pub fn clean_working_set() -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};

        unsafe {
            let handle = GetCurrentProcess();
            let _ = SetProcessWorkingSetSize(handle, usize::MAX, usize::MAX);
            log::info!("Working set cleaned");
        }
    }
    Ok(())
}

/// 获取当前进程内存使用（MB）。用 psapi 直接查询，非阻塞（不 spawn 子进程）。
pub fn get_memory_usage_mb() -> f64 {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::System::ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;

        let mut pmc: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            GetProcessMemoryInfo(
                GetCurrentProcess(),
                &mut pmc,
                std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            )
        };
        if ok != 0 {
            return pmc.WorkingSetSize as f64 / (1024.0 * 1024.0);
        }
    }
    0.0
}
