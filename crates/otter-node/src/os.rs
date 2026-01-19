//! Node.js `os` module implementation.
//!
//! Provides operating system-related utility methods and properties.
//!
//! # Example
//!
//! ```javascript
//! const os = require('os');
//!
//! console.log(os.platform());  // 'darwin', 'linux', 'win32'
//! console.log(os.arch());      // 'x64', 'arm64'
//! console.log(os.cpus());      // CPU information
//! console.log(os.totalmem());  // Total memory in bytes
//! console.log(os.freemem());   // Free memory in bytes
//! console.log(os.homedir());   // Home directory path
//! console.log(os.tmpdir());    // Temp directory path
//! console.log(os.hostname());  // System hostname
//! console.log(os.EOL);         // End-of-line marker
//! ```

use serde::{Deserialize, Serialize};
use std::env;

/// CPU core information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuInfo {
    /// CPU model name.
    pub model: String,

    /// CPU speed in MHz.
    pub speed: u64,

    /// CPU time statistics.
    pub times: CpuTimes,
}

/// CPU time statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuTimes {
    /// Time spent in user mode (ms).
    pub user: u64,

    /// Time spent in nice mode (ms).
    pub nice: u64,

    /// Time spent in system mode (ms).
    pub sys: u64,

    /// Time spent idle (ms).
    pub idle: u64,

    /// Time spent in IRQ mode (ms).
    pub irq: u64,
}

impl Default for CpuTimes {
    fn default() -> Self {
        Self {
            user: 0,
            nice: 0,
            sys: 0,
            idle: 0,
            irq: 0,
        }
    }
}

/// Network interface information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkInterface {
    /// Network interface address.
    pub address: String,

    /// Network mask.
    pub netmask: String,

    /// Address family (IPv4 or IPv6).
    pub family: String,

    /// MAC address.
    pub mac: String,

    /// Whether this is an internal interface.
    pub internal: bool,

    /// CIDR notation (e.g., "192.168.1.1/24").
    pub cidr: Option<String>,
}

/// User information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInfo {
    /// User ID.
    pub uid: i32,

    /// Group ID.
    pub gid: i32,

    /// Username.
    pub username: String,

    /// Home directory.
    pub homedir: String,

    /// Default shell.
    pub shell: Option<String>,
}

/// Operating system type information.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsType {
    Darwin,
    Linux,
    Windows,
    Unknown,
}

impl OsType {
    /// Get OS type as string (Node.js compatible).
    pub fn as_str(&self) -> &'static str {
        match self {
            OsType::Darwin => "Darwin",
            OsType::Linux => "Linux",
            OsType::Windows => "Windows_NT",
            OsType::Unknown => "Unknown",
        }
    }
}

/// Get the operating system platform.
///
/// Returns 'darwin', 'linux', or 'win32'.
pub fn platform() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "darwin"
    }
    #[cfg(target_os = "linux")]
    {
        "linux"
    }
    #[cfg(target_os = "windows")]
    {
        "win32"
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        "unknown"
    }
}

/// Get the operating system type.
///
/// Returns 'Darwin', 'Linux', or 'Windows_NT'.
pub fn os_type() -> OsType {
    #[cfg(target_os = "macos")]
    {
        OsType::Darwin
    }
    #[cfg(target_os = "linux")]
    {
        OsType::Linux
    }
    #[cfg(target_os = "windows")]
    {
        OsType::Windows
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        OsType::Unknown
    }
}

/// Get the CPU architecture.
///
/// Returns 'x64', 'arm64', 'ia32', or 'arm'.
pub fn arch() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    {
        "x64"
    }
    #[cfg(target_arch = "aarch64")]
    {
        "arm64"
    }
    #[cfg(target_arch = "x86")]
    {
        "ia32"
    }
    #[cfg(target_arch = "arm")]
    {
        "arm"
    }
    #[cfg(not(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "x86",
        target_arch = "arm"
    )))]
    {
        "unknown"
    }
}

/// Get the operating system release version.
#[cfg(target_os = "macos")]
pub fn release() -> String {
    use std::process::Command;
    Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(target_os = "linux")]
pub fn release() -> String {
    std::fs::read_to_string("/proc/version")
        .map(|v| v.split_whitespace().nth(2).unwrap_or("unknown").to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(target_os = "windows")]
pub fn release() -> String {
    "10.0".to_string() // Simplified
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn release() -> String {
    "unknown".to_string()
}

/// Get the operating system version.
#[cfg(target_os = "macos")]
pub fn version() -> String {
    use std::process::Command;
    Command::new("sw_vers")
        .arg("-buildVersion")
        .output()
        .map(|o| {
            format!(
                "Darwin Kernel Version {}",
                String::from_utf8_lossy(&o.stdout).trim()
            )
        })
        .unwrap_or_else(|_| "Darwin Kernel Version unknown".to_string())
}

#[cfg(target_os = "linux")]
pub fn version() -> String {
    std::fs::read_to_string("/proc/version")
        .unwrap_or_else(|_| "Linux version unknown".to_string())
        .trim()
        .to_string()
}

#[cfg(target_os = "windows")]
pub fn version() -> String {
    "Windows 10".to_string() // Simplified
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn version() -> String {
    "unknown".to_string()
}

/// Get the system hostname.
pub fn hostname() -> String {
    #[cfg(unix)]
    {
        use std::ffi::CStr;
        let mut buf = [0i8; 256];
        // SAFETY: gethostname is a standard POSIX function
        unsafe {
            if libc::gethostname(buf.as_mut_ptr(), buf.len()) == 0 {
                CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
            } else {
                "localhost".to_string()
            }
        }
    }
    #[cfg(not(unix))]
    {
        env::var("COMPUTERNAME").unwrap_or_else(|_| "localhost".to_string())
    }
}

/// Get the current user's home directory.
pub fn homedir() -> String {
    #[cfg(unix)]
    {
        env::var("HOME").unwrap_or_else(|_| "/".to_string())
    }
    #[cfg(windows)]
    {
        env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".to_string())
    }
    #[cfg(not(any(unix, windows)))]
    {
        ".".to_string()
    }
}

/// Get the operating system's default directory for temporary files.
pub fn tmpdir() -> String {
    env::temp_dir().to_string_lossy().into_owned()
}

/// Get the endianness of the CPU.
///
/// Returns 'BE' for big-endian or 'LE' for little-endian.
pub fn endianness() -> &'static str {
    if cfg!(target_endian = "big") {
        "BE"
    } else {
        "LE"
    }
}

/// Get the total amount of system memory in bytes.
#[cfg(target_os = "macos")]
pub fn totalmem() -> u64 {
    use std::mem;
    let mut size: u64 = 0;
    let mut len = mem::size_of::<u64>();
    let mut mib = [libc::CTL_HW, libc::HW_MEMSIZE];
    // SAFETY: sysctl is a standard BSD function
    unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            &mut size as *mut _ as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        );
    }
    size
}

#[cfg(target_os = "linux")]
pub fn totalmem() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|content| content.lines().find(|line| line.starts_with("MemTotal:")))
        .and_then(|line| {
            line.split_whitespace()
                .nth(1)
                .and_then(|s| s.parse::<u64>().ok())
        })
        .map(|kb| kb * 1024) // Convert KB to bytes
        .unwrap_or(0)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn totalmem() -> u64 {
    0
}

/// Get the amount of free system memory in bytes.
#[cfg(target_os = "macos")]
pub fn freemem() -> u64 {
    use std::mem;

    let mut stats: libc::vm_statistics64 = unsafe { mem::zeroed() };
    let mut count = (mem::size_of::<libc::vm_statistics64>() / mem::size_of::<libc::c_int>())
        as libc::mach_msg_type_number_t;

    // SAFETY: host_statistics64 is a standard Mach function
    // Note: mach_host_self is deprecated in libc but there's no replacement in mach2 crate
    #[allow(deprecated)]
    unsafe {
        let host = libc::mach_host_self();
        libc::host_statistics64(
            host,
            libc::HOST_VM_INFO64,
            &mut stats as *mut _ as *mut _,
            &mut count,
        );
    }

    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    (stats.free_count as u64) * page_size
}

#[cfg(target_os = "linux")]
pub fn freemem() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|content| {
            content
                .lines()
                .find(|line| line.starts_with("MemAvailable:"))
                .or_else(|| content.lines().find(|line| line.starts_with("MemFree:")))
        })
        .and_then(|line| {
            line.split_whitespace()
                .nth(1)
                .and_then(|s| s.parse::<u64>().ok())
        })
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn freemem() -> u64 {
    0
}

/// Get information about each CPU/core.
#[cfg(target_os = "macos")]
pub fn cpus() -> Vec<CpuInfo> {
    // Get CPU count
    let num_cpus = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) } as usize;

    // Get CPU model
    let model = {
        use std::ffi::CStr;
        let mut buf = [0i8; 256];
        let mut len = buf.len();
        let name = b"machdep.cpu.brand_string\0";
        // SAFETY: sysctlbyname is a standard BSD function
        unsafe {
            libc::sysctlbyname(
                name.as_ptr() as *const i8,
                buf.as_mut_ptr() as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            );
            CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
        }
    };

    // Get CPU speed (MHz)
    let speed = {
        let mut freq: u64 = 0;
        let mut len = std::mem::size_of::<u64>();
        let name = b"hw.cpufrequency\0";
        // SAFETY: sysctlbyname is a standard BSD function
        unsafe {
            libc::sysctlbyname(
                name.as_ptr() as *const i8,
                &mut freq as *mut _ as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            );
        }
        freq / 1_000_000 // Convert Hz to MHz
    };

    (0..num_cpus)
        .map(|_| CpuInfo {
            model: model.clone(),
            speed,
            times: CpuTimes::default(),
        })
        .collect()
}

#[cfg(target_os = "linux")]
pub fn cpus() -> Vec<CpuInfo> {
    let mut cpus = Vec::new();

    if let Ok(content) = std::fs::read_to_string("/proc/cpuinfo") {
        let mut model = String::new();
        let mut speed: u64 = 0;

        for line in content.lines() {
            if line.starts_with("model name") {
                if let Some(value) = line.split(':').nth(1) {
                    model = value.trim().to_string();
                }
            } else if line.starts_with("cpu MHz") {
                if let Some(value) = line.split(':').nth(1) {
                    speed = value.trim().parse().unwrap_or(0) as u64;
                }
            } else if line.is_empty() && !model.is_empty() {
                cpus.push(CpuInfo {
                    model: model.clone(),
                    speed,
                    times: CpuTimes::default(),
                });
            }
        }

        // Handle last CPU if no trailing newline
        if !model.is_empty() && cpus.is_empty() {
            cpus.push(CpuInfo {
                model,
                speed,
                times: CpuTimes::default(),
            });
        }
    }

    if cpus.is_empty() {
        // Fallback
        let num_cpus = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) } as usize;
        for _ in 0..num_cpus {
            cpus.push(CpuInfo {
                model: "Unknown".to_string(),
                speed: 0,
                times: CpuTimes::default(),
            });
        }
    }

    cpus
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn cpus() -> Vec<CpuInfo> {
    vec![CpuInfo {
        model: "Unknown".to_string(),
        speed: 0,
        times: CpuTimes::default(),
    }]
}

/// Get the system uptime in seconds.
#[cfg(target_os = "macos")]
pub fn uptime() -> u64 {
    use std::mem;
    let mut boottime: libc::timeval = unsafe { mem::zeroed() };
    let mut len = mem::size_of::<libc::timeval>();
    let mut mib = [libc::CTL_KERN, libc::KERN_BOOTTIME];

    // SAFETY: sysctl is a standard BSD function
    unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            &mut boottime as *mut _ as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        );
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    now - boottime.tv_sec as u64
}

#[cfg(target_os = "linux")]
pub fn uptime() -> u64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|content| {
            content
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<f64>().ok())
        })
        .map(|secs| secs as u64)
        .unwrap_or(0)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn uptime() -> u64 {
    0
}

/// Get the load average (1, 5, 15 minutes).
#[cfg(unix)]
pub fn loadavg() -> [f64; 3] {
    let mut avg = [0.0f64; 3];
    // SAFETY: getloadavg is a standard POSIX function
    unsafe {
        libc::getloadavg(avg.as_mut_ptr(), 3);
    }
    avg
}

#[cfg(not(unix))]
pub fn loadavg() -> [f64; 3] {
    [0.0, 0.0, 0.0]
}

/// Get the end-of-line marker for the current platform.
pub fn eol() -> &'static str {
    #[cfg(windows)]
    {
        "\r\n"
    }
    #[cfg(not(windows))]
    {
        "\n"
    }
}

/// Get the scheduling priority of a process.
#[cfg(unix)]
pub fn getpriority(pid: i32) -> i32 {
    // SAFETY: getpriority is a standard POSIX function
    unsafe { libc::getpriority(libc::PRIO_PROCESS, pid as u32) }
}

#[cfg(not(unix))]
pub fn getpriority(_pid: i32) -> i32 {
    0
}

/// Set the scheduling priority of a process.
#[cfg(unix)]
pub fn setpriority(pid: i32, priority: i32) -> Result<(), std::io::Error> {
    // SAFETY: setpriority is a standard POSIX function
    let result = unsafe { libc::setpriority(libc::PRIO_PROCESS, pid as u32, priority) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
pub fn setpriority(_pid: i32, _priority: i32) -> Result<(), std::io::Error> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "setpriority not supported on this platform",
    ))
}

/// Get user information.
#[cfg(unix)]
pub fn userinfo() -> UserInfo {
    // SAFETY: getuid, getgid are standard POSIX functions
    let uid = unsafe { libc::getuid() } as i32;
    let gid = unsafe { libc::getgid() } as i32;
    let username = env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    let homedir = homedir();
    let shell = env::var("SHELL").ok();

    UserInfo {
        uid,
        gid,
        username,
        homedir,
        shell,
    }
}

#[cfg(not(unix))]
pub fn userinfo() -> UserInfo {
    UserInfo {
        uid: 0,
        gid: 0,
        username: env::var("USERNAME").unwrap_or_else(|_| "unknown".to_string()),
        homedir: homedir(),
        shell: None,
    }
}

/// Get machine type (hardware identifier).
#[cfg(target_os = "macos")]
pub fn machine() -> String {
    use std::ffi::CStr;
    let mut buf = [0i8; 256];
    let mut len = buf.len();
    let mut mib = [libc::CTL_HW, libc::HW_MACHINE];
    // SAFETY: sysctl is a standard BSD function
    unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        );
        CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
    }
}

#[cfg(target_os = "linux")]
pub fn machine() -> String {
    use std::ffi::CStr;
    let mut info: libc::utsname = unsafe { std::mem::zeroed() };
    // SAFETY: uname is a standard POSIX function
    unsafe {
        libc::uname(&mut info);
        CStr::from_ptr(info.machine.as_ptr())
            .to_string_lossy()
            .into_owned()
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn machine() -> String {
    "unknown".to_string()
}

/// Generate JavaScript code for the os module.
pub fn os_module_js() -> &'static str {
    r#"
(function() {
    const osModule = {
        arch: () => globalThis.__os_arch,
        cpus: () => globalThis.__os_cpus,
        endianness: () => globalThis.__os_endianness,
        freemem: () => globalThis.__os_freemem,
        homedir: () => globalThis.__os_homedir,
        hostname: () => globalThis.__os_hostname,
        loadavg: () => globalThis.__os_loadavg,
        machine: () => globalThis.__os_machine,
        platform: () => globalThis.__os_platform,
        release: () => globalThis.__os_release,
        tmpdir: () => globalThis.__os_tmpdir,
        totalmem: () => globalThis.__os_totalmem,
        type: () => globalThis.__os_type,
        uptime: () => globalThis.__os_uptime,
        userInfo: () => globalThis.__os_userinfo,
        version: () => globalThis.__os_version,

        // Constants
        EOL: globalThis.__os_eol,
        devNull: globalThis.__os_devnull,

        // Priority constants
        constants: {
            priority: {
                PRIORITY_LOW: 19,
                PRIORITY_BELOW_NORMAL: 10,
                PRIORITY_NORMAL: 0,
                PRIORITY_ABOVE_NORMAL: -7,
                PRIORITY_HIGH: -14,
                PRIORITY_HIGHEST: -20,
            },
            signals: {},
            errno: {},
        },

        // Network interfaces (stub for now)
        networkInterfaces: () => ({}),

        // Priority functions
        getPriority: (pid) => {
            if (globalThis.__os_getpriority) {
                return globalThis.__os_getpriority(pid || 0);
            }
            return 0;
        },
        setPriority: (pid, priority) => {
            if (globalThis.__os_setpriority) {
                globalThis.__os_setpriority(pid || 0, priority);
            }
        },
    };

    // Register with module system if available
    if (globalThis.__registerNodeBuiltin) {
        globalThis.__registerNodeBuiltin('os', osModule);
    }

    globalThis.__osModule = osModule;
})();
"#
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform() {
        let p = platform();
        assert!(["darwin", "linux", "win32", "unknown"].contains(&p));
    }

    #[test]
    fn test_arch() {
        let a = arch();
        assert!(["x64", "arm64", "ia32", "arm", "unknown"].contains(&a));
    }

    #[test]
    fn test_homedir() {
        let home = homedir();
        assert!(!home.is_empty());
    }

    #[test]
    fn test_tmpdir() {
        let tmp = tmpdir();
        assert!(!tmp.is_empty());
    }

    #[test]
    fn test_hostname() {
        let h = hostname();
        assert!(!h.is_empty());
    }

    #[test]
    fn test_endianness() {
        let e = endianness();
        assert!(e == "BE" || e == "LE");
    }

    #[test]
    fn test_eol() {
        let e = eol();
        assert!(e == "\n" || e == "\r\n");
    }

    #[test]
    fn test_os_type() {
        let t = os_type();
        assert!(
            [
                OsType::Darwin,
                OsType::Linux,
                OsType::Windows,
                OsType::Unknown
            ]
            .contains(&t)
        );
    }

    #[test]
    fn test_totalmem() {
        let mem = totalmem();
        // Should be at least 1GB on any modern system
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        assert!(mem > 1_000_000_000);
    }

    #[test]
    fn test_cpus() {
        let cpus = cpus();
        assert!(!cpus.is_empty());
    }

    #[test]
    fn test_loadavg() {
        let avg = loadavg();
        // Load average should be non-negative
        assert!(avg[0] >= 0.0);
        assert!(avg[1] >= 0.0);
        assert!(avg[2] >= 0.0);
    }

    #[test]
    fn test_userinfo() {
        let info = userinfo();
        assert!(!info.username.is_empty());
        assert!(!info.homedir.is_empty());
    }

    #[test]
    fn test_js_code_generation() {
        let js = os_module_js();
        assert!(js.contains("arch"));
        assert!(js.contains("platform"));
        assert!(js.contains("cpus"));
        assert!(js.contains("freemem"));
        assert!(js.contains("totalmem"));
    }
}
