use anyhow::{bail, Result};
use std::ffi::CString;
use std::os::raw::c_char;
use std::path::PathBuf;

#[repr(C)]
struct VzVmConfig {
    kernel_path: *const c_char,
    initrd_path: *const c_char,
    cmdline: *const c_char,
    disk_path: *const c_char,
    share_dir: *const c_char,
    share_tag: *const c_char,
    share_read_only: bool,
    logcat_path: *const c_char,
    cpu_count: u32,
    memory_mb: u64,
    display_width: u32,
    display_height: u32,
    adb_host_port: u16,
    adb_guest_vsock_port: u32,
    enable_audio: bool,
    headless: bool,
    serial_stdin: bool,
}

extern "C" {
    fn vz_is_supported() -> bool;
    fn vz_run_vm(cfg: *const VzVmConfig) -> i32;
}

pub struct VmLaunchOptions {
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub cmdline: String,
    pub disk: PathBuf,
    pub share_dir: Option<PathBuf>,
    pub share_tag: Option<String>,
    pub share_read_only: bool,
    /// Host file receiving the guest's /dev/hvc2 (logcat) stream.
    pub logcat_path: Option<PathBuf>,
    pub cpus: u32,
    pub memory_mb: u64,
    pub width: u32,
    pub height: u32,
    pub adb_host_port: u16,
    pub adb_guest_vsock_port: u32,
    pub enable_audio: bool,
    pub headless: bool,
    pub serial_stdin: bool,
}

pub fn is_supported() -> bool {
    unsafe { vz_is_supported() }
}

pub fn run_vm(opts: &VmLaunchOptions) -> Result<()> {
    if !is_supported() {
        bail!("Apple Virtualization.framework is not supported on this host");
    }

    let kernel_c = CString::new(opts.kernel.to_string_lossy().as_bytes())?;
    let initrd_c = CString::new(opts.initrd.to_string_lossy().as_bytes())?;
    let cmdline_c = CString::new(opts.cmdline.as_str())?;
    let disk_c = CString::new(opts.disk.to_string_lossy().as_bytes())?;
    let share_dir_c = match &opts.share_dir {
        Some(p) => Some(CString::new(p.to_string_lossy().as_bytes())?),
        None => None,
    };
    let share_tag_c = match &opts.share_tag {
        Some(t) => Some(CString::new(t.as_str())?),
        None => None,
    };
    let logcat_c = match &opts.logcat_path {
        Some(p) => Some(CString::new(p.to_string_lossy().as_bytes())?),
        None => None,
    };

    let raw = VzVmConfig {
        kernel_path: kernel_c.as_ptr(),
        initrd_path: initrd_c.as_ptr(),
        cmdline: cmdline_c.as_ptr(),
        disk_path: disk_c.as_ptr(),
        share_dir: share_dir_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
        share_tag: share_tag_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
        share_read_only: opts.share_read_only,
        logcat_path: logcat_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
        cpu_count: opts.cpus,
        memory_mb: opts.memory_mb,
        display_width: opts.width,
        display_height: opts.height,
        adb_host_port: opts.adb_host_port,
        adb_guest_vsock_port: opts.adb_guest_vsock_port,
        enable_audio: opts.enable_audio,
        headless: opts.headless,
        serial_stdin: opts.serial_stdin,
    };

    let rc = unsafe { vz_run_vm(&raw) };
    if rc != 0 {
        bail!("VM exited with status {rc}");
    }
    Ok(())
}
