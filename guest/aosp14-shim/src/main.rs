//! Tiny static aarch64-unknown-linux-musl PID 1 shim for `aosp14-runner`.
//!
//! Booted via `rdinit=/aosp14-shim` from a small cpio overlay added to the
//! unmodified AOSP 14 initrd:
//!   1. As PID 1 (early_virtio_setup): loads the vendor ramdisk's virtio_pci +
//!      virtio_console modules so the kernel log reaches the host on /dev/hvc0
//!      from the very start, binds the per-hvc console devices one at a time
//!      so /dev/hvcN matches Cuttlefish's numbering, and opens the three
//!      secure_env ports before any HAL can write to them.
//!   2. Forks a background helper process.
//!   3. PID 1 `execv("/init", argv)` so unmodified Android 14 first-stage init
//!      runs with zero interference.
//!   4. The helper grabs an open directory FD on `/proc` as soon as
//!      `first_stage_init` mounts `/proc` (before `SwitchRoot` moves it), waits
//!      for second-stage init's property service, chroots into `/proc/1/root`
//!      and keeps eth0 (`ro.vendor.disable_rename_eth0`). Then:
//!        - a child waits until `on fs` mounts the host `virtiofs` share at
//!          `/mnt/vendor/shared` and execs Cuttlefish's unmodified musl
//!          `secure_env` (via `libc_musl.so`) on the already-open ports
//!          (`se_gatekeeper`, `se_oemlock`, `se_keymint`);
//!        - a child switches Bluetooth and NFC off after boot (no host-side
//!          rootcanal/casimir, see POST_BOOT_SCRIPT);
//!        - the helper itself serves the host control channel on
//!          vsock:CONTROL_VSOCK_PORT (`shutdown` / `reboot` -> `sys.powerctl`).

use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

const SHARE_ROOT: &str = "/mnt/vendor/shared";
const SECURE_ENV_BIN: &str = "/mnt/vendor/shared/bin/secure_env";
const LIBC_MUSL: &str = "/mnt/vendor/shared/lib64/libc_musl.so";
const LIB_DIR: &str = "/mnt/vendor/shared/lib64";
const TPM_DIR: &str = "/mnt/vendor/shared/tpm";
const CF_CONFIG: &str = "/mnt/vendor/shared/cuttlefish_config.json";
const SE_LOG: &str = "/mnt/vendor/shared/cf/instances/cvd-1/logs/secure_env.log";
const PROPERTY_SERVICE: &str = "/dev/socket/property_service";
/// Named non-console virtio ports wired by the host to hvc4/hvc10/hvc11.
const SE_PORTS: [&str; 3] = ["se_gatekeeper", "se_oemlock", "se_keymint"];
/// Must match `CONTROL_VSOCK_PORT` in native/vz_bridge.m.
const CONTROL_VSOCK_PORT: u32 = 6680;
const AF_VSOCK: i32 = 40;
/// bionic's PROP_MSG_SETPROP2 (property service protocol v2).
const PROP_MSG_SETPROP2: u32 = 0x0002_0001;

fn kmsg(msg: &str) {
    if let Ok(mut f) = OpenOptions::new().write(true).open("/dev/kmsg") {
        // One write() per record: /dev/kmsg turns every write into a log line.
        let _ = f.write_all(format!("<6>aosp14-shim: {msg}\n").as_bytes());
    }
}

fn cstr(s: &str) -> CString {
    CString::new(s).expect("cstring")
}

fn wait_for_proc_fd() -> Option<i32> {
    let deadline = Instant::now() + Duration::from_secs(15);
    let proc_self = cstr("/proc/self");
    let proc_dir = cstr("/proc");
    while Instant::now() < deadline {
        if unsafe { libc::access(proc_self.as_ptr(), libc::F_OK) } == 0 {
            let fd = unsafe {
                libc::open(
                    proc_dir.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                )
            };
            if fd >= 0 {
                return Some(fd);
            }
        }
        thread::sleep(Duration::from_millis(2));
    }
    None
}

/// Wait for second-stage init (its property service socket only ever exists in
/// the final root, never in the initramfs or /first_stage_ramdisk) and chroot
/// into init's root.
fn wait_for_android_root_and_chroot(proc_fd: i32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(60);
    let rel_marker = cstr(&format!("1/root{PROPERTY_SERVICE}"));
    let rel_root = cstr("1/root");
    while Instant::now() < deadline {
        if unsafe { libc::faccessat(proc_fd, rel_marker.as_ptr(), libc::F_OK, 0) } == 0 {
            let new_root = unsafe {
                libc::openat(
                    proc_fd,
                    rel_root.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                )
            };
            if new_root >= 0 {
                let rc1 = unsafe { libc::fchdir(new_root) };
                let dot = cstr(".");
                let slash = cstr("/");
                let rc2 = unsafe { libc::chroot(dot.as_ptr()) };
                let rc3 = unsafe { libc::chdir(slash.as_ptr()) };
                unsafe {
                    libc::close(new_root);
                    libc::close(proc_fd);
                }
                return rc1 == 0 && rc2 == 0 && rc3 == 0;
            }
        }
        thread::sleep(Duration::from_millis(15));
    }
    unsafe { libc::close(proc_fd) };
    false
}

fn find_vport_by_name(target: &str) -> Option<String> {
    let entries = fs::read_dir("/sys/class/virtio-ports").ok()?;
    for entry in entries.flatten() {
        let name_path = entry.path().join("name");
        if let Ok(content) = fs::read_to_string(&name_path) {
            if content.trim() == target {
                if let Some(dev_name) = entry.file_name().to_str() {
                    let dev_path = format!("/dev/{dev_name}");
                    if Path::new(&dev_path).exists() {
                        return Some(dev_path);
                    }
                }
            }
        }
    }
    None
}

fn wait_for_vport(target: &str) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(path) = find_vport_by_name(target) {
            return Some(path);
        }
        thread::sleep(Duration::from_millis(20));
    }
    None
}

fn open_rw_pair(path: &str) -> Option<(i32, i32)> {
    let cpath = cstr(path);
    let fd_in = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
    if fd_in < 0 {
        return None;
    }
    let fd_out = unsafe { libc::dup(fd_in) };
    if fd_out < 0 {
        unsafe { libc::close(fd_in) };
        return None;
    }
    Some((fd_in, fd_out))
}

fn make_idle_socketpair() -> Option<i32> {
    let mut sv = [-1i32; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sv.as_mut_ptr()) } != 0 {
        return None;
    }
    // Intentionally keep sv[1] open (non-CLOEXEC) across exec so sv[0] never sees EOF.
    Some(sv[0])
}

fn make_idle_pipe_read() -> Option<i32> {
    let mut fds = [-1i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return None;
    }
    // Intentionally keep fds[1] open (non-CLOEXEC) across exec so fds[0] never sees EOF.
    Some(fds[0])
}

fn make_idle_unix_listener() -> Option<i32> {
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return None;
    }
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    // Linux abstract Unix socket namespace: sun_path[0] == 0.
    let name = b"\0aosp14_confui_dummy";
    for (i, &b) in name.iter().enumerate() {
        addr.sun_path[i] = b as libc::c_char;
    }
    let addr_len = (std::mem::size_of::<libc::sa_family_t>() + name.len()) as libc::socklen_t;
    let rc = unsafe {
        libc::bind(
            fd,
            (&addr as *const libc::sockaddr_un) as *const libc::sockaddr,
            addr_len,
        )
    };
    if rc != 0 || unsafe { libc::listen(fd, 4) } != 0 {
        unsafe { libc::close(fd) };
        return None;
    }
    Some(fd)
}

fn wait_for_path(path: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if Path::new(path).exists() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Set an Android system property through init's property service socket.
fn set_property(name: &str, value: &str) -> bool {
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return false;
    }
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (i, &b) in PROPERTY_SERVICE.as_bytes().iter().enumerate() {
        addr.sun_path[i] = b as libc::c_char;
    }
    let ok = unsafe {
        libc::connect(
            fd,
            (&addr as *const libc::sockaddr_un).cast(),
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    } == 0;
    let mut result = -1i32;
    if ok {
        let mut msg = PROP_MSG_SETPROP2.to_ne_bytes().to_vec();
        for s in [name, value] {
            msg.extend_from_slice(&(s.len() as u32).to_ne_bytes());
            msg.extend_from_slice(s.as_bytes());
        }
        let sent = unsafe { libc::send(fd, msg.as_ptr().cast(), msg.len(), libc::MSG_NOSIGNAL) };
        if sent == msg.len() as isize {
            let mut buf = [0u8; 4];
            if unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), 4, libc::MSG_WAITALL) } == 4 {
                result = i32::from_ne_bytes(buf);
            }
        }
    }
    unsafe { libc::close(fd) };
    result == 0
}

/// Serve the host control channel: one command per connection.
fn run_control_listener() -> ! {
    let fd = unsafe { libc::socket(AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    addr.svm_family = AF_VSOCK as libc::sa_family_t;
    addr.svm_port = CONTROL_VSOCK_PORT;
    addr.svm_cid = libc::VMADDR_CID_ANY;
    let bound = fd >= 0
        && unsafe {
            libc::bind(
                fd,
                (&addr as *const libc::sockaddr_vm).cast(),
                std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
            )
        } == 0
        && unsafe { libc::listen(fd, 4) } == 0;
    if !bound {
        kmsg(&format!(
            "control channel: cannot listen on vsock:{CONTROL_VSOCK_PORT}: {}",
            std::io::Error::last_os_error()
        ));
        unsafe { libc::_exit(20) };
    }
    kmsg(&format!("control channel listening on vsock:{CONTROL_VSOCK_PORT}"));
    loop {
        let conn = unsafe { libc::accept4(fd, std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC) };
        if conn < 0 {
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        let mut buf = [0u8; 64];
        let n = unsafe { libc::read(conn, buf.as_mut_ptr().cast(), buf.len()) };
        let cmd = if n > 0 {
            String::from_utf8_lossy(&buf[..n as usize]).trim().to_string()
        } else {
            String::new()
        };
        let ok = match cmd.as_str() {
            "shutdown" | "reboot" => {
                kmsg(&format!("host requested {cmd}"));
                set_property("sys.powerctl", &cmd)
            }
            _ => false,
        };
        let reply: &[u8] = if ok { b"ok\n" } else { b"error\n" };
        unsafe {
            libc::write(conn, reply.as_ptr().cast(), reply.len());
            libc::close(conn);
        }
    }
}

fn run_helper(early_ports: Option<[i32; 3]>) -> ! {
    // Detach session so signals from console never disturb secure_env.
    unsafe { libc::setsid() };

    let proc_fd = match wait_for_proc_fd() {
        Some(fd) => fd,
        None => unsafe { libc::_exit(1) },
    };

    if !wait_for_android_root_and_chroot(proc_fd) {
        kmsg("timed out waiting for Android second-stage init");
        unsafe { libc::_exit(2) };
    }

    // Networking: Cuttlefish renames eth0 to buried_eth0 at post-fs-data and
    // expects Wi-Fi through mac80211_hwsim radios served by a host-side
    // wmediumd/OpenWRT access point, which this runner does not provide.
    // Keeping eth0 lets Android's Ethernet support run DHCP against the VZ NAT.
    // The read-only property can still be set here: second-stage init has just
    // started and post-fs-data is far away.
    if set_property("ro.vendor.disable_rename_eth0", "1") {
        kmsg("keeping eth0 (Android Ethernet over the VZ NAT)");
    } else {
        kmsg("could not set ro.vendor.disable_rename_eth0; eth0 will be renamed (no network)");
    }

    if unsafe { libc::fork() } == 0 {
        run_secure_env(early_ports);
    }
    for fd in early_ports.into_iter().flatten() {
        unsafe { libc::close(fd) };
    }
    if unsafe { libc::fork() } == 0 {
        run_post_boot_fixups();
    }
    run_control_listener();
}

/// Cuttlefish's Bluetooth (hvc5) and NFC (hvc12) HALs talk to host-side
/// emulators (rootcanal, casimir) that this runner does not provide. Their
/// framework stacks abort on the first unanswered command and are restarted
/// forever, so switch both radios off once Android has booted. The setting
/// persists in /data; repeating it on later boots is a no-op. NFC needs
/// retries: NfcService saves the setting asynchronously and its crash-looping
/// process often dies before the write lands. BluetoothManagerService also
/// starts the stack on every boot until it has learned the adapter name and
/// address, which never happens without a controller, so seed placeholders.
const POST_BOOT_SCRIPT: &str = "\
while [ \"$(getprop sys.boot_completed)\" != 1 ]; do sleep 2; done
cmd bluetooth_manager disable
if [ \"$(settings get secure bluetooth_address)\" = null ]; then
  settings put secure bluetooth_name \"$(getprop ro.product.model)\"
  settings put secure bluetooth_address 02:00:00:00:00:00
fi
i=0
while [ $i -lt 30 ]; do
  dumpsys nfc 2>/dev/null | grep -q 'mState=off' && break
  svc nfc disable
  sleep 2
  i=$((i + 1))
done
log -t aosp14-shim \"Bluetooth and NFC switched off after $i NFC attempts (no rootcanal/casimir on this host)\"
";

/// Find a running process by argv[0]; returns its /proc directory.
fn find_process(argv0: &[&str], timeout: Duration) -> Option<std::path::PathBuf> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        for e in fs::read_dir("/proc").ok()?.flatten() {
            let Ok(cmdline) = fs::read(e.path().join("cmdline")) else { continue };
            let first = cmdline.split(|&b| b == 0).next().unwrap_or_default();
            if argv0.iter().any(|a| a.as_bytes() == first) {
                return Some(e.path());
            }
        }
        thread::sleep(Duration::from_millis(500));
    }
    None
}

fn run_post_boot_fixups() -> ! {
    // Run like an app_process user: in zygote's mount namespace (init's
    // "default" namespace, which has all APEXes; this helper was forked into
    // the bootstrap one) and with zygote's environment (init's exports incl.
    // BOOTCLASSPATH, which /proc/1/environ does not show).
    let Some(zygote) = find_process(&["zygote64", "zygote"], Duration::from_secs(300)) else {
        kmsg("post-boot fixups: zygote never started");
        unsafe { libc::_exit(1) };
    };
    let ns = cstr(&format!("{}/ns/mnt", zygote.display()));
    let ns_fd = unsafe { libc::open(ns.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if ns_fd < 0 || unsafe { libc::setns(ns_fd, libc::CLONE_NEWNS) } != 0 {
        kmsg(&format!("post-boot fixups: setns failed: {}", std::io::Error::last_os_error()));
        unsafe { libc::_exit(1) };
    }
    unsafe {
        libc::close(ns_fd);
        libc::chdir(cstr("/").as_ptr());
    }
    let environ = fs::read(zygote.join("environ")).unwrap_or_default();
    let env: Vec<CString> = environ
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .filter_map(|s| CString::new(s).ok())
        .collect();
    let mut envp: Vec<*const libc::c_char> = env.iter().map(|s| s.as_ptr()).collect();
    envp.push(std::ptr::null());
    let args = [cstr("/system/bin/sh"), cstr("-c"), cstr(POST_BOOT_SCRIPT)];
    let mut argv: Vec<*const libc::c_char> = args.iter().map(|s| s.as_ptr()).collect();
    argv.push(std::ptr::null());
    unsafe {
        let null = libc::open(cstr("/dev/null").as_ptr(), libc::O_RDWR);
        if null >= 0 {
            for fd in 0..3 {
                libc::dup2(null, fd);
            }
        }
        libc::execve(args[0].as_ptr(), argv.as_ptr(), envp.as_ptr());
    }
    kmsg(&format!("post-boot fixups: exec /system/bin/sh failed: {}", std::io::Error::last_os_error()));
    unsafe { libc::_exit(127) }
}

/// Open the secure_env ports while still PID 1, long before Android starts any
/// HAL. virtio_console silently discards host data arriving on a non-console
/// port that no process has open, so a HAL request sent before secure_env
/// opened its port would be lost and KeyMint (then keystore2, vold and init's
/// `mount_all`) would wait forever. An open port queues the data instead.
/// Returns the fds in SE_PORTS order; they are inherited by the helper.
/// Requires sysfs at /sys.
fn open_se_ports_early() -> Option<[i32; 3]> {
    let mut fds = [-1i32; 3];
    let deadline = Instant::now() + Duration::from_secs(5);
    'ports: for (i, name) in SE_PORTS.iter().enumerate() {
        loop {
            if let Some(fd) = open_vport_node(name) {
                fds[i] = fd;
                break;
            }
            if Instant::now() >= deadline {
                kmsg(&format!("virtio-port {name} not found"));
                break 'ports;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
    if fds.iter().any(|&fd| fd < 0) {
        for fd in fds.into_iter().filter(|&fd| fd >= 0) {
            unsafe { libc::close(fd) };
        }
        return None;
    }
    Some(fds)
}

/// Create a private device node for the named virtio port and open it.
/// The node lives in the initramfs /dev, which first-stage init later hides
/// under its own tmpfs; the open fd stays valid.
fn open_vport_node(name: &str) -> Option<i32> {
    for entry in fs::read_dir("/sys/class/virtio-ports").ok()?.flatten() {
        let dir = entry.path();
        // Unnamed (console) ports have no `name` attribute at all.
        match fs::read_to_string(dir.join("name")) {
            Ok(n) if n.trim() == name => {}
            _ => continue,
        }
        let devno = fs::read_to_string(dir.join("dev")).ok()?;
        let (maj, min) = devno.trim().split_once(':')?;
        let (maj, min): (u32, u32) = (maj.parse().ok()?, min.parse().ok()?);
        let node = cstr(&format!("/dev/aosp14-{name}"));
        unsafe {
            libc::mkdir(cstr("/dev").as_ptr(), 0o755);
            libc::unlink(node.as_ptr());
            libc::mknod(node.as_ptr(), libc::S_IFCHR | 0o600, libc::makedev(maj, min));
        }
        let fd = unsafe { libc::open(node.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
        return (fd >= 0).then_some(fd);
    }
    None
}

/// (in, out) fd pairs for gatekeeper, oemlock and keymint, in SE_PORTS order.
fn se_port_pairs(early: Option<[i32; 3]>) -> [(i32, i32); 3] {
    let mut pairs = [(-1, -1); 3];
    for (i, name) in SE_PORTS.iter().enumerate() {
        pairs[i] = match early {
            Some(fds) => (fds[i], unsafe { libc::dup(fds[i]) }),
            None => {
                // Late fallback: racy against the HALs, see open_se_ports_early().
                let path = wait_for_vport(name).unwrap_or_else(|| {
                    kmsg(&format!("missing virtio-port {name}"));
                    unsafe { libc::_exit(3) };
                });
                open_rw_pair(&path).unwrap_or_else(|| {
                    kmsg(&format!("failed to open {path}"));
                    unsafe { libc::_exit(4) };
                })
            }
        };
    }
    pairs
}

fn run_secure_env(early_ports: Option<[i32; 3]>) -> ! {
    // `on fs` mounts the virtiofs share shortly after the property service starts.
    if !wait_for_path(SECURE_ENV_BIN, Duration::from_secs(60)) {
        kmsg("secure_env not staged on the virtiofs share; KeyMint/Gatekeeper unavailable");
        unsafe { libc::_exit(0) };
    }

    kmsg(&format!(
        "found {SECURE_ENV_BIN}; ports opened {}",
        if early_ports.is_some() { "early (as PID 1)" } else { "late (racy)" }
    ));
    let [(gk_in, gk_out), (ol_in, ol_out), (km_in, km_out)] = se_port_pairs(early_ports);

    let kmas_fd = make_idle_socketpair().unwrap_or_else(|| unsafe { libc::_exit(9) });
    let snap_fd = make_idle_socketpair().unwrap_or_else(|| unsafe { libc::_exit(10) });
    let kevt_fd = make_idle_pipe_read().unwrap_or_else(|| unsafe { libc::_exit(11) });
    let confui_fd = make_idle_unix_listener().unwrap_or_else(|| unsafe { libc::_exit(12) });

    let _ = fs::create_dir_all(TPM_DIR);
    let _ = fs::create_dir_all(Path::new(SE_LOG).parent().unwrap_or(Path::new(SHARE_ROOT)));

    let tpm_c = cstr(TPM_DIR);
    if unsafe { libc::chdir(tpm_c.as_ptr()) } != 0 {
        kmsg("chdir to TPM_DIR failed");
    }

    // Redirect stdout/stderr to persistent log file on host virtiofs share.
    let log_c = cstr(SE_LOG);
    let log_fd = unsafe {
        libc::open(
            log_c.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
            0o644,
        )
    };
    if log_fd >= 0 {
        unsafe {
            libc::dup2(log_fd, 1);
            libc::dup2(log_fd, 2);
            if log_fd > 2 {
                libc::close(log_fd);
            }
        }
    }

    let args_str = vec![
        LIBC_MUSL.to_string(),
        "--library-path".to_string(),
        LIB_DIR.to_string(),
        SECURE_ENV_BIN.to_string(),
        "--tpm_impl=in_memory".to_string(),
        "--keymint_impl=tpm".to_string(),
        "--gatekeeper_impl=tpm".to_string(),
        "--oemlock_impl=tpm".to_string(),
        format!("--keymint_fd_in={km_in}"),
        format!("--keymint_fd_out={km_out}"),
        format!("--keymaster_fd_in={kmas_fd}"),
        format!("--keymaster_fd_out={kmas_fd}"),
        format!("--gatekeeper_fd_in={gk_in}"),
        format!("--gatekeeper_fd_out={gk_out}"),
        format!("--oemlock_fd_in={ol_in}"),
        format!("--oemlock_fd_out={ol_out}"),
        format!("--confui_server_fd={confui_fd}"),
        format!("--snapshot_control_fd={snap_fd}"),
        format!("--kernel_events_fd={kevt_fd}"),
    ];

    let env_str = vec![
        format!("CUTTLEFISH_CONFIG_FILE={CF_CONFIG}"),
        "CUTTLEFISH_INSTANCE=1".to_string(),
        "HOME=/mnt/vendor/shared/tpm".to_string(),
    ];

    let c_args: Vec<CString> = args_str.iter().map(|s| cstr(s)).collect();
    let mut argv_ptrs: Vec<*const libc::c_char> = c_args.iter().map(|s| s.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());

    let c_env: Vec<CString> = env_str.iter().map(|s| cstr(s)).collect();
    let mut envp_ptrs: Vec<*const libc::c_char> = c_env.iter().map(|s| s.as_ptr()).collect();
    envp_ptrs.push(std::ptr::null());

    kmsg("exec'ing Cuttlefish secure_env via libc_musl.so");
    unsafe {
        libc::execve(c_args[0].as_ptr(), argv_ptrs.as_ptr(), envp_ptrs.as_ptr());
    }
    let err = std::io::Error::last_os_error();
    kmsg(&format!("execve(secure_env) failed: {err}"));
    unsafe { libc::_exit(127) }
}

/// Virtio console devices (VIRTIO_ID_CONSOLE) as shown in sysfs.
const VIRTIO_ID_CONSOLE: &str = "0x0003";
const VIRTIO_BUS: &str = "/sys/bus/virtio";
const DEBUGFS_PORTS: &str = "/sys/kernel/debug/virtio-ports";

fn kernel_major_minor() -> Option<(u32, u32)> {
    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut uts) } != 0 {
        return None;
    }
    let rel = unsafe { std::ffi::CStr::from_ptr(uts.release.as_ptr()) }.to_string_lossy().into_owned();
    parse_major_minor(&rel)
}

fn parse_major_minor(s: &str) -> Option<(u32, u32)> {
    let mut it = s.split(|c: char| !c.is_ascii_digit());
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

/// Same rule as Android first-stage init: prefer /lib/modules/<major>.<minor>*,
/// fall back to /lib/modules.
fn module_dir() -> Option<String> {
    let base = "/lib/modules";
    if let (Some(kv), Ok(entries)) = (kernel_major_minor(), fs::read_dir(base)) {
        let mut dirs: Vec<String> = entries
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter(|n| parse_major_minor(n) == Some(kv))
            .collect();
        dirs.sort();
        for d in dirs {
            let p = format!("{base}/{d}");
            if Path::new(&format!("{p}/modules.dep")).is_file() {
                return Some(p);
            }
        }
    }
    Path::new(&format!("{base}/modules.dep")).is_file().then(|| base.to_string())
}

fn insmod(path: &str) -> bool {
    let cpath = cstr(path);
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return false;
    }
    let empty = cstr("");
    let rc = unsafe { libc::syscall(libc::SYS_finit_module, fd, empty.as_ptr(), 0) };
    let err = std::io::Error::last_os_error().raw_os_error();
    unsafe { libc::close(fd) };
    rc == 0 || err == Some(libc::EEXIST)
}

/// basename -> (path, deps) from modules.dep; deps load in reverse order.
type ModuleDeps = std::collections::HashMap<String, (String, Vec<String>)>;

fn read_module_deps() -> Option<ModuleDeps> {
    let dir = module_dir()?;
    let dep_text = fs::read_to_string(format!("{dir}/modules.dep")).ok()?;
    let resolve = |p: &str| if p.starts_with('/') { p.to_string() } else { format!("{dir}/{p}") };
    let mut deps = ModuleDeps::new();
    for line in dep_text.lines() {
        let Some((m, rest)) = line.split_once(':') else { continue };
        let base = m.rsplit('/').next().unwrap_or(m).to_string();
        let list: Vec<String> = rest.split_whitespace().map(resolve).collect();
        deps.insert(base, (resolve(m), list));
    }
    Some(deps)
}

fn load_module(deps: &ModuleDeps, name: &str) -> bool {
    let Some((path, list)) = deps.get(name) else {
        kmsg(&format!("module {name} not in modules.dep"));
        return false;
    };
    for dep in list.iter().rev() {
        insmod(dep);
    }
    let ok = insmod(path);
    if !ok {
        kmsg(&format!("early insmod {path} failed"));
    }
    ok
}

fn write_sysfs(path: &str, value: &str) -> bool {
    fs::write(path, value).is_ok()
}

fn mount_fs(fstype: &str, target: &str) -> bool {
    let (t, ty) = (cstr(target), cstr(fstype));
    unsafe { libc::mkdir(t.as_ptr(), 0o755) };
    unsafe { libc::mount(ty.as_ptr(), t.as_ptr(), ty.as_ptr(), 0, std::ptr::null()) == 0 }
}

/// Console-type virtio devices ("virtioN"), sorted by PCI address.
fn console_devices_in_pci_order() -> Vec<String> {
    let mut devs: Vec<(String, String)> = fs::read_dir(format!("{VIRTIO_BUS}/devices"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            fs::read_to_string(e.path().join("device")).map(|d| d.trim() == VIRTIO_ID_CONSOLE).unwrap_or(false)
        })
        .filter_map(|e| {
            // .../0000:00:05.0/virtio3 -> "0000:00:05.0"
            let real = fs::canonicalize(e.path()).ok()?;
            let pci = real.parent()?.file_name()?.to_str()?.to_string();
            Some((pci, e.file_name().to_str()?.to_string()))
        })
        .collect();
    devs.sort();
    devs.into_iter().map(|(_, v)| v).collect()
}

fn debugfs_port_field(port: &str, field: &str) -> Option<String> {
    let text = fs::read_to_string(format!("{DEBUGFS_PORTS}/{port}")).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix(field)?.strip_prefix(':').map(|v| v.trim().to_string()))
}

/// Wait until the device's port has been set up: either registered as an hvc
/// console, or (named secure_env device) it is a plain named port.
fn wait_console_registered(dev: &str, have_debugfs: bool) {
    let deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < deadline {
        let ports: Vec<String> = fs::read_dir(format!("{VIRTIO_BUS}/devices/{dev}/virtio-ports"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .collect();
        if let Some(p) = ports.first() {
            if !have_debugfs {
                thread::sleep(Duration::from_millis(50));
                return;
            }
            if debugfs_port_field(p, "is_console").as_deref() == Some("yes") {
                return;
            }
            let named = fs::read_to_string(format!("/sys/class/virtio-ports/{p}/name"))
                .map(|n| n.trim().starts_with("se_"))
                .unwrap_or(false);
            if named {
                return;
            }
        }
        thread::sleep(Duration::from_millis(2));
    }
    kmsg(&format!("{dev}: console port did not come up in time"));
}

/// Check that every port named "cf-hvcN" really is /dev/hvcN.
fn verify_hvc_numbering() {
    let Ok(entries) = fs::read_dir("/sys/class/virtio-ports") else { return };
    let mut bad = Vec::new();
    for e in entries.flatten() {
        let port = e.file_name().to_string_lossy().into_owned();
        let Ok(name) = fs::read_to_string(e.path().join("name")) else { continue };
        let Some(want) = name.trim().strip_prefix("cf-hvc").and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        // debugfs vtermno is 1-based: the first console port is hvc0.
        let got = debugfs_port_field(&port, "console_vtermno").and_then(|v| v.parse::<u32>().ok());
        if got != Some(want + 1) {
            bad.push(format!("cf-hvc{want}->vterm{got:?}"));
        }
    }
    if bad.is_empty() {
        kmsg("hvc numbering verified");
    } else {
        kmsg(&format!("WARNING: hvc numbering is off, HALs will be cross-wired: {}", bad.join(" ")));
    }
}

/// Bring up the virtio consoles before Android runs:
///  - GKI builds virtio_pci and virtio_console as vendor modules and
///    Virtualization.framework has no UART, so without this the host sees no
///    kernel output until first-stage init loads them (nothing at all if boot
///    stalls earlier). Android's libmodprobe treats the later EEXIST as success.
///  - /dev/hvcN numbers follow the order in which console ports register. The
///    host gives every hvc its own device; binding them one at a time in PCI
///    order makes device N /dev/hvcN, which the Cuttlefish HALs hardcode.
///  - opens the secure_env ports (see open_se_ports_early).
fn early_virtio_setup() -> Option<[i32; 3]> {
    if !mount_fs("sysfs", "/sys") {
        kmsg("cannot mount sysfs; skipping early virtio setup");
        return None;
    }
    let have_debugfs = mount_fs("debugfs", "/sys/kernel/debug");
    let mut ports = None;
    if let Some(deps) = read_module_deps() {
        load_module(&deps, "virtio_pci.ko");
        // Keep virtio_console from binding everything at once on load.
        let manual = write_sysfs(&format!("{VIRTIO_BUS}/drivers_autoprobe"), "0");
        load_module(&deps, "virtio_console.ko");
        if manual {
            for dev in console_devices_in_pci_order() {
                write_sysfs(&format!("{VIRTIO_BUS}/drivers/virtio_console/bind"), &dev);
                wait_console_registered(&dev, have_debugfs);
            }
            write_sysfs(&format!("{VIRTIO_BUS}/drivers_autoprobe"), "1");
        }
        if have_debugfs {
            verify_hvc_numbering();
        }
        ports = open_se_ports_early();
    }
    if have_debugfs {
        unsafe { libc::umount2(cstr("/sys/kernel/debug").as_ptr(), libc::MNT_DETACH) };
    }
    unsafe { libc::umount2(cstr("/sys").as_ptr(), libc::MNT_DETACH) };
    ports
}

fn main() {
    // We run as PID 1 before Android first-stage init, which mounts a fresh
    // tmpfs over /dev; this node only lives in the initramfs.
    let kmsg_path = cstr("/dev/kmsg");
    unsafe {
        libc::mknod(kmsg_path.as_ptr(), libc::S_IFCHR | 0o600, libc::makedev(1, 11));
    }
    let early_ports = early_virtio_setup();
    kmsg(&format!(
        "early console ready; secure_env ports {}; starting Android first-stage init",
        if early_ports.is_some() { "held open" } else { "unavailable" }
    ));

    let pid = unsafe { libc::fork() };
    if pid == 0 {
        run_helper(early_ports);
    }
    // /init must not inherit the ports (the helper keeps its own copies).
    for fd in early_ports.into_iter().flatten() {
        unsafe { libc::close(fd) };
    }

    // PID 1: immediately hand off to unmodified Android 14 /init.
    let init_path = cstr("/init");
    let orig_args: Vec<CString> = std::env::args_os()
        .enumerate()
        .map(|(i, a)| {
            if i == 0 {
                cstr("/init")
            } else {
                CString::new(a.as_bytes()).unwrap_or_else(|_| cstr(""))
            }
        })
        .collect();
    let mut argv: Vec<*const libc::c_char> = orig_args.iter().map(|s| s.as_ptr()).collect();
    if argv.is_empty() {
        argv.push(init_path.as_ptr());
    }
    argv.push(std::ptr::null());

    unsafe {
        libc::execv(init_path.as_ptr(), argv.as_ptr());
        libc::_exit(126);
    }
}
