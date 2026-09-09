//! Checked OS counters. No shell tools, process arguments, or environment in output.
use super::{CpuTicks, MemoryStats, ProcessCounter};
use std::time::Instant;

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    pub(in crate::http_api::machine_stats) fn process_ids(
        limit: usize,
    ) -> Result<Vec<u32>, String> {
        let mut pids = vec![0i32; limit];
        let count = unsafe {
            libc::proc_listallpids(
                pids.as_mut_ptr().cast(),
                std::mem::size_of_val(pids.as_slice()) as i32,
            )
        };
        if count <= 0 {
            return Err(format!(
                "proc_listallpids unavailable: {}",
                std::io::Error::last_os_error()
            ));
        }
        pids.truncate((count as usize).min(limit));
        Ok(pids
            .into_iter()
            .filter(|p| *p >= 0)
            .map(|p| p as u32)
            .collect())
    }

    // mach_host_self creates a send right on each call. Release it even on errors.
    unsafe extern "C" {
        // libSystem exports this Mach API, but libc 0.2 does not declare it.
        fn mach_port_deallocate(
            task: libc::mach_port_t,
            name: libc::mach_port_t,
        ) -> libc::kern_return_t;
    }
    struct Host(libc::mach_port_t);
    impl Host {
        #[allow(deprecated)] // Existing libc Mach bindings; no additional runtime dependency.
        fn new() -> Self {
            Self(unsafe { libc::mach_host_self() })
        }
    }
    impl Drop for Host {
        fn drop(&mut self) {
            #[allow(deprecated)]
            unsafe {
                mach_port_deallocate(libc::mach_task_self(), self.0);
            }
        }
    }

    fn sysctl<T: Copy>(name: &std::ffi::CStr) -> Result<T, String> {
        let mut value = std::mem::MaybeUninit::<T>::zeroed();
        let mut size = std::mem::size_of::<T>();
        let result = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if result != 0 || size != std::mem::size_of::<T>() {
            return Err(format!(
                "{} unavailable: {}",
                name.to_string_lossy(),
                std::io::Error::last_os_error()
            ));
        }
        Ok(unsafe { value.assume_init() })
    }

    pub(in crate::http_api::machine_stats) fn cpu_ticks() -> Result<CpuTicks, String> {
        let host = Host::new();
        let mut info = std::mem::MaybeUninit::<libc::host_cpu_load_info>::zeroed();
        let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
        let result = unsafe {
            libc::host_statistics(
                host.0,
                libc::HOST_CPU_LOAD_INFO,
                info.as_mut_ptr().cast(),
                &mut count,
            )
        };
        if result != libc::KERN_SUCCESS || count != libc::HOST_CPU_LOAD_INFO_COUNT {
            return Err(format!(
                "host_statistics CPU unavailable (Mach status {result})"
            ));
        }
        let ticks = unsafe { info.assume_init() }.cpu_ticks;
        Ok(CpuTicks {
            // Keep nice separate until after subtraction so a single counter wrap is detected.
            values: [
                ticks[0] as u64,
                ticks[3] as u64,
                ticks[1] as u64,
                ticks[2] as u64,
                0,
                0,
                0,
                0,
            ],
            logical_cores: sysctl::<u32>(c"hw.logicalcpu")? as usize,
            source: "host_statistics/HOST_CPU_LOAD_INFO",
            has_wait: false,
        })
    }

    #[allow(deprecated)] // Keep the same native Mach ABI as the existing sysinfo dependency.
    fn seconds_per_mach_tick() -> Result<f64, String> {
        static SCALE: std::sync::OnceLock<Result<f64, String>> = std::sync::OnceLock::new();
        SCALE
            .get_or_init(|| {
                let mut timebase = libc::mach_timebase_info_data_t { numer: 0, denom: 0 };
                #[allow(deprecated)]
                let result = unsafe { libc::mach_timebase_info(&mut timebase) };
                if result != libc::KERN_SUCCESS {
                    return Err(format!("Mach timebase unavailable (status {result})"));
                }
                super::seconds_per_tick(timebase.numer, timebase.denom)
            })
            .clone()
    }

    pub(in crate::http_api::machine_stats) fn process_counter(
        pid: u32,
    ) -> Result<ProcessCounter, String> {
        let mut info = std::mem::MaybeUninit::<libc::proc_taskallinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_taskallinfo>();
        let result = unsafe {
            libc::proc_pidinfo(
                pid as i32,
                libc::PROC_PIDTASKALLINFO,
                0,
                info.as_mut_ptr().cast(),
                size as i32,
            )
        };
        if result != size as i32 {
            return Err("process exited or proc_pidinfo denied/unavailable".into());
        }
        let info = unsafe { info.assume_init() };
        let name = if info.pbsd.pbi_name[0] == 0 {
            &info.pbsd.pbi_comm[..]
        } else {
            &info.pbsd.pbi_name[..]
        };
        let bytes: Vec<u8> = name
            .iter()
            .take_while(|c| **c != 0)
            .map(|c| *c as u8)
            .collect();
        Ok(ProcessCounter {
            pid,
            parent_pid: info.pbsd.pbi_ppid,
            name: String::from_utf8_lossy(&bytes).into_owned(),
            identity: (info.pbsd.pbi_start_tvsec, info.pbsd.pbi_start_tvusec),
            cpu_seconds: (info.ptinfo.pti_total_user as f64 + info.ptinfo.pti_total_system as f64)
                * seconds_per_mach_tick()?,
            resident_bytes: info.ptinfo.pti_resident_size,
            at: Instant::now(),
        })
    }

    pub(in crate::http_api::machine_stats) fn memory() -> Result<MemoryStats, String> {
        let host = Host::new();
        let mut info = std::mem::MaybeUninit::<libc::vm_statistics64>::zeroed();
        let mut count = libc::HOST_VM_INFO64_COUNT;
        let result = unsafe {
            libc::host_statistics64(
                host.0,
                libc::HOST_VM_INFO64,
                info.as_mut_ptr().cast(),
                &mut count,
            )
        };
        if result != libc::KERN_SUCCESS || count != libc::HOST_VM_INFO64_COUNT {
            return Err(format!(
                "host_statistics64 memory unavailable (Mach status {result})"
            ));
        }
        let vm = unsafe { info.assume_init() };
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page_size <= 0 {
            return Err("page size unavailable".into());
        }
        let page_size = page_size as u64;
        let total = sysctl::<u64>(c"hw.memsize")?;
        let mut errors = Vec::new();
        let pressure = match sysctl::<i32>(c"kern.memorystatus_vm_pressure_level") {
            Ok(1) => Some("normal".into()),
            Ok(2) => Some("warning".into()),
            Ok(4) => Some("critical".into()),
            Ok(value) => {
                errors.push(format!("unknown memory pressure level {value}"));
                None
            }
            Err(error) => {
                errors.push(error);
                None
            }
        };
        let swap = match sysctl::<libc::xsw_usage>(c"vm.swapusage") {
            Ok(value) => Some(value),
            Err(error) => {
                errors.push(error);
                None
            }
        };
        // Preserve sysinfo 0.33's existing macOS memory semantics. Available is
        // an estimate, not total-used; document the formula beside the API.
        Ok(MemoryStats {
            total_bytes: total,
            used_bytes: (u64::from(vm.active_count)
                + u64::from(vm.wire_count)
                + u64::from(vm.compressor_page_count)
                + u64::from(vm.speculative_count))
                * page_size,
            free_bytes: u64::from(vm.free_count).saturating_sub(vm.speculative_count.into())
                * page_size,
            available_bytes: (u64::from(vm.free_count)
                + u64::from(vm.inactive_count)
                + u64::from(vm.purgeable_count))
            .saturating_sub(vm.compressor_page_count.into())
                * page_size,
            pressure,
            swap_total_bytes: swap.as_ref().map(|s| s.xsu_total),
            swap_used_bytes: swap.as_ref().map(|s| s.xsu_used),
            compressed_bytes: Some(u64::from(vm.compressor_page_count) * page_size),
            source: Some("host_statistics64; sysctl; available is sysinfo-0.33 estimate".into()),
            collection_errors: Some(errors),
        })
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    pub(in crate::http_api::machine_stats) fn process_ids(
        limit: usize,
    ) -> Result<Vec<u32>, String> {
        let entries =
            std::fs::read_dir("/proc").map_err(|e| format!("process enumeration: {e}"))?;
        let mut pids = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| format!("process enumeration: {e}"))?;
            if let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            {
                pids.push(pid);
                if pids.len() == limit {
                    break;
                }
            }
        }
        Ok(pids)
    }

    pub(in crate::http_api::machine_stats) fn cpu_ticks() -> Result<CpuTicks, String> {
        parse_cpu_stat(
            &std::fs::read_to_string("/proc/stat").map_err(|e| format!("/proc/stat: {e}"))?,
        )
    }

    fn parse_cpu_stat(text: &str) -> Result<CpuTicks, String> {
        let mut fields = text
            .lines()
            .next()
            .ok_or("missing /proc/stat CPU row")?
            .split_whitespace();
        if fields.next() != Some("cpu") {
            return Err("missing aggregate CPU row".into());
        }
        let mut values = [0; 8];
        for value in &mut values {
            *value = fields
                .next()
                .ok_or("incomplete CPU counters")?
                .parse()
                .map_err(|_| "invalid CPU counter")?;
        }
        // guest/guest_nice already belong to user/nice. Never count them twice.
        let logical_cores = text
            .lines()
            .filter(|line| {
                line.strip_prefix("cpu")
                    .is_some_and(|tail| tail.starts_with(|c: char| c.is_ascii_digit()))
            })
            .count();
        Ok(CpuTicks {
            values,
            logical_cores,
            source: "/proc/stat",
            has_wait: true,
        })
    }

    pub(in crate::http_api::machine_stats) fn process_counter(
        pid: u32,
    ) -> Result<ProcessCounter, String> {
        let text = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map_err(|_| "process exited or /proc stat denied/unavailable")?;
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if hz <= 0 || page_size <= 0 {
            return Err("process counter units unavailable".into());
        }
        parse_process_stat(pid, &text, hz as f64, page_size as u64)
    }

    fn parse_process_stat(
        pid: u32,
        text: &str,
        hz: f64,
        page_size: u64,
    ) -> Result<ProcessCounter, String> {
        let start = text.find('(').ok_or("missing process name")?;
        let end = text.rfind(')').ok_or("missing process name terminator")?;
        if end <= start {
            return Err("invalid process name".into());
        }
        let fields: Vec<_> = text[end + 1..].split_whitespace().collect();
        let number = |index: usize| -> Result<u64, String> {
            fields
                .get(index)
                .ok_or_else(|| "incomplete process stat".to_string())?
                .parse()
                .map_err(|_| "invalid process stat".into())
        };
        Ok(ProcessCounter {
            pid,
            parent_pid: number(1)? as u32,
            name: text[start + 1..end].into(),
            identity: (number(19)?, 0),
            cpu_seconds: (number(11)? as f64 + number(12)? as f64) / hz,
            resident_bytes: number(21)?
                .checked_mul(page_size)
                .ok_or("resident bytes overflow")?,
            at: Instant::now(),
        })
    }

    pub(in crate::http_api::machine_stats) fn memory() -> Result<MemoryStats, String> {
        let text =
            std::fs::read_to_string("/proc/meminfo").map_err(|e| format!("/proc/meminfo: {e}"))?;
        let bytes = |key: &str| -> Result<u64, String> {
            let line = text
                .lines()
                .find_map(|line| line.strip_prefix(key))
                .ok_or_else(|| format!("{key} unavailable"))?;
            let mut fields = line.split_whitespace();
            let kb: u64 = fields
                .next()
                .ok_or("missing memory value")?
                .parse()
                .map_err(|_| "invalid memory value")?;
            if fields.next() != Some("kB") {
                return Err("unexpected memory units".into());
            }
            kb.checked_mul(1024)
                .ok_or_else(|| "memory counter overflow".into())
        };
        let total = bytes("MemTotal:")?;
        let available = bytes("MemAvailable:")?;
        let swap_total = bytes("SwapTotal:")?;
        Ok(MemoryStats {
            total_bytes: total,
            used_bytes: total.checked_sub(available).ok_or("invalid available memory")?,
            free_bytes: bytes("MemFree:")?, available_bytes: available,
            pressure: None,
            swap_total_bytes: Some(swap_total),
            swap_used_bytes: Some(swap_total.checked_sub(bytes("SwapFree:")?).ok_or("invalid swap free")?),
            compressed_bytes: None,
            source: Some("/proc/meminfo; MemAvailable kernel estimate".into()),
            collection_errors: Some(vec!["categorical memory pressure and system-wide compressed bytes are not exposed on Linux by this collector".into()]),
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn proc_cpu_ignores_duplicate_guest_counters() {
            let cpu = parse_cpu_stat("cpu 10 20 30 40 5 6 7 8 999 999\ncpu0 0\ncpu1 0\n").unwrap();
            assert_eq!(cpu.values, [10, 20, 30, 40, 5, 6, 7, 8]);
            assert_eq!(cpu.logical_cores, 2);
            assert!(parse_cpu_stat("cpu 1 2").is_err());
        }
        #[test]
        fn process_name_can_contain_spaces_and_parentheses() {
            let text = "42 (worker (test)) S 1 0 0 0 0 0 0 0 0 0 200 100 0 0 0 0 0 0 99 0 7";
            let process = parse_process_stat(42, text, 100.0, 4096).unwrap();
            assert_eq!(process.name, "worker (test)");
            assert_eq!(process.cpu_seconds, 3.0);
            assert_eq!(process.identity, (99, 0));
            assert_eq!(process.resident_bytes, 7 * 4096);
        }
    }
}

pub(super) use platform::{cpu_ticks, memory, process_counter, process_ids};

// Mach task CPU times are absolute-time ticks, not nanoseconds on Apple Silicon.
#[cfg(any(target_os = "macos", test))]
fn seconds_per_tick(numer: u32, denom: u32) -> Result<f64, String> {
    if numer == 0 || denom == 0 {
        return Err("invalid Mach timebase".into());
    }
    Ok(f64::from(numer) / f64::from(denom) / 1e9)
}

#[cfg(test)]
mod unit_tests {
    #[test]
    fn apple_silicon_and_intel_task_time_units() {
        let silicon = super::seconds_per_tick(125, 3).unwrap();
        assert!((24_000_000.0 * silicon - 1.0).abs() < 1e-12);
        assert_eq!(
            1_000_000_000.0 * super::seconds_per_tick(1, 1).unwrap(),
            1.0
        );
        assert!(super::seconds_per_tick(1, 0).is_err());
        assert!(super::seconds_per_tick(0, 1).is_err());
    }
}
