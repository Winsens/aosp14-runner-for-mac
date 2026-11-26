mod bootimg;
mod gpt;
mod vz;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use gpt::{PartitionSource, PartitionSpec};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// adbd listens on vsock:<port> as well as tcp:<port> whenever
/// `service.adb.tcp.port` is set (Cuttlefish sets 5555), so the host forwards
/// straight to adbd. Cuttlefish's `socket_vsock_proxy` (vsock:6520 -> tcp:5555)
/// is avoided on purpose: in android-14 it reads an uninitialized `started_`
/// flag on every accepted connection and randomly aborts.
const ADB_VSOCK_PORT: u32 = 5555;
/// Cuttlefish lights HAL: a vsock server bound to these bootconfig values. Unset,
/// it binds cid 0, panics, and system_server blocks in StartLightsService.
const VMADDR_CID_ANY: u32 = u32::MAX;
const LIGHTS_VSOCK_PORT: u32 = 6900;
const VK_API_VERSION_1_2: u32 = (1 << 22) | (2 << 12); // 4202496

/// Workdir artifacts produced by `prepare` (ramdisks are uncompressed newc cpio).
const KERNEL_FILE: &str = "kernel";
const INIT_RAMDISK_FILE: &str = "init_ramdisk.cpio";
const VENDOR_RAMDISK_FILE: &str = "vendor_ramdisk.cpio";
/// Partitions an existing disk.img must have; older layouts lacked `frp`.
const REQUIRED_PARTITIONS: &[&str] = &["misc", "frp", "super", "metadata", "userdata"];

#[derive(Parser)]
#[command(
    name = "aosp14-runner",
    about = "Native macOS Apple Silicon runner for unmodified AOSP 14 (aosp_cf_arm64_only_phone) images"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Check host support and prepared artifacts
    Doctor {
        #[arg(long, default_value = "./workdir")]
        workdir: PathBuf,
        #[arg(long, default_value = "./images")]
        images_dir: PathBuf,
    },

    /// Unpack AOSP 14 Cuttlefish images and build the GPT disk.img
    Prepare {
        /// Directory containing boot.img, init_boot.img, vendor_boot.img, super.img, vbmeta*.img
        #[arg(long, default_value = "./images")]
        images_dir: PathBuf,
        /// Output workspace directory
        #[arg(long, default_value = "./workdir")]
        workdir: PathBuf,
        /// Userdata partition size in GiB
        #[arg(long, default_value_t = 8)]
        userdata_gb: u64,
    },

    /// Boot AOSP 14 inside Apple Virtualization.framework
    Run(RunArgs),
}

#[derive(Args)]
struct RunArgs {
    #[arg(long, default_value = "./workdir")]
    workdir: PathBuf,
    /// Used for auto-prepare if workdir is not yet prepared
    #[arg(long, default_value = "./images")]
    images_dir: PathBuf,
    #[arg(long, default_value_t = 4)]
    cpus: u32,
    #[arg(long, default_value_t = 4096)]
    memory_mb: u64,
    #[arg(long, default_value_t = 720)]
    width: u32,
    #[arg(long, default_value_t = 1280)]
    height: u32,
    #[arg(long, default_value_t = 320)]
    density: u32,
    #[arg(long, default_value_t = 15555)]
    adb_port: u16,
    /// Cuttlefish hwcomposer HAL: `ranchu` (DRM display finder) or `drm`
    #[arg(long, default_value = "ranchu")]
    hwcomposer: String,
    /// Cuttlefish fstab suffix (e.g. cf.f2fs.hctr2 or cf.ext4.hctr2)
    #[arg(long, default_value = "cf.f2fs.hctr2")]
    fstab_suffix: String,
    /// No-op, kept for compatibility: eth0 is now always kept as Ethernet (the
    /// in-guest shim sets ro.vendor.disable_rename_eth0; a bootconfig value only
    /// reaches ro.boot.*, so this flag never had an effect).
    #[arg(long, default_value_t = false, hide = true)]
    ethernet_only: bool,
    /// Run SELinux in enforcing mode instead of permissive (experimental: the
    /// in-guest shim and secure_env run unconfined in the `kernel` domain)
    #[arg(long, default_value_t = false)]
    enforcing: bool,
    /// Disable VirtIO sound device
    #[arg(long, default_value_t = false)]
    no_audio: bool,
    /// Recreate disk.img from images_dir before booting
    #[arg(long, default_value_t = false)]
    wipe_data: bool,
    #[arg(long, default_value_t = 8)]
    userdata_gb: u64,
    #[arg(long, default_value_t = false)]
    headless: bool,
    #[arg(long, default_value_t = false)]
    serial_stdin: bool,
    /// Extra kernel command line arguments
    #[arg(long)]
    extra_cmdline: Option<String>,
    /// Extra bootconfig lines (semicolon-separated, e.g. "k1=v1;k2=v2")
    #[arg(long)]
    extra_bootconfig: Option<String>,
    /// Override path to static aarch64-musl `aosp14-shim` binary
    #[arg(long)]
    shim_bin: Option<PathBuf>,
}

fn has_vz_entitlement() -> Option<bool> {
    let exe = std::env::current_exe().ok()?;
    let out = Command::new("codesign")
        .args(["-d", "--entitlements", "-", "--xml"])
        .arg(&exe)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string()
        + &String::from_utf8_lossy(&out.stderr);
    Some(text.contains("com.apple.security.virtualization"))
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    })
}

fn required_images_present(images_dir: &Path) -> bool {
    ["boot.img", "init_boot.img", "vendor_boot.img", "super.img"]
        .iter()
        .all(|f| images_dir.join(f).is_file())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src).with_context(|| format!("reading {}", src.display()))? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &dst_path)?;
        } else if ty.is_file() {
            fs::copy(entry.path(), &dst_path).with_context(|| {
                format!("copying {} -> {}", entry.path().display(), dst_path.display())
            })?;
        }
    }
    Ok(())
}

fn prepare_secure_env_share(images_dir: &Path, workdir: &Path, wipe_tpm: bool) -> Result<bool> {
    let src_se = images_dir.join("secure_env");
    if !src_se.join("bin/secure_env").is_file() {
        return Ok(false);
    }
    let share_dir = workdir.join("share");
    if wipe_tpm {
        let _ = fs::remove_dir_all(share_dir.join("tpm"));
    }
    if !share_dir.join("bin/secure_env").is_file() {
        println!(
            "[prepare] Staging secure_env musl runtime -> {}",
            share_dir.display()
        );
        copy_dir_recursive(&src_se, &share_dir)?;
    }
    // Create TPM persistence dir and Cuttlefish config/log directories.
    fs::create_dir_all(share_dir.join("tpm"))?;
    fs::create_dir_all(share_dir.join("cf/instances/cvd-1/logs"))?;
    fs::create_dir_all(share_dir.join("cf/uds/cvd-1/internal"))?;
    let cf_cfg = r#"{
  "root_dir": "/mnt/vendor/shared/cf",
  "instances_uds_dir": "/mnt/vendor/shared/cf/uds",
  "instances": {
    "1": {
      "run_as_daemon": false
    }
  }
}
"#;
    fs::write(share_dir.join("cuttlefish_config.json"), cf_cfg)?;
    Ok(true)
}

fn find_shim_binary(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        if p.is_file() {
            return Some(p.to_path_buf());
        }
    }
    let mut candidates = vec![
        PathBuf::from("./assets/aosp14-shim"),
        PathBuf::from("./guest/aosp14-shim/target/aarch64-unknown-linux-musl/release/aosp14-shim"),
    ];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(root) = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent()) {
            candidates.push(root.join("assets/aosp14-shim"));
        }
    }
    candidates.into_iter().find(|p| p.is_file())
}

fn check_required_images(images_dir: &Path) -> Result<()> {
    for f in ["boot.img", "init_boot.img", "vendor_boot.img", "super.img"] {
        let p = images_dir.join(f);
        if !p.is_file() {
            bail!(
                "Missing required image: {} (build aosp_cf_arm64_only_phone-ap2a-userdebug first)",
                p.display()
            );
        }
    }
    Ok(())
}

fn boot_artifacts_present(workdir: &Path) -> bool {
    [KERNEL_FILE, INIT_RAMDISK_FILE, VENDOR_RAMDISK_FILE]
        .iter()
        .all(|f| workdir.join(f).is_file())
}

/// Extract the kernel and the (decompressed) generic + vendor ramdisks.
/// Leaves disk.img (and therefore userdata) untouched.
fn unpack_boot_images(images_dir: &Path, workdir: &Path) -> Result<()> {
    check_required_images(images_dir)?;
    fs::create_dir_all(workdir)?;

    println!("[prepare] Unpacking boot.img, init_boot.img, vendor_boot.img ...");
    let boot = bootimg::parse_boot_img(&images_dir.join("boot.img"))?;
    let init_boot = bootimg::parse_boot_img(&images_dir.join("init_boot.img"))?;
    let vendor_boot = bootimg::parse_vendor_boot_img(&images_dir.join("vendor_boot.img"))?;

    if boot.kernel.is_empty() {
        bail!("boot.img contains no kernel");
    }
    if init_boot.ramdisk.is_empty() {
        bail!("init_boot.img contains no ramdisk");
    }
    if vendor_boot.ramdisk.is_empty() {
        bail!("vendor_boot.img contains no vendor ramdisk");
    }
    let init_cpio =
        bootimg::decompress_ramdisk(&init_boot.ramdisk).context("init_boot.img ramdisk")?;

    fs::write(workdir.join(KERNEL_FILE), &boot.kernel)?;
    fs::write(workdir.join(INIT_RAMDISK_FILE), &init_cpio)?;
    fs::write(workdir.join(VENDOR_RAMDISK_FILE), &vendor_boot.ramdisk)?;
    fs::write(workdir.join("vendor_cmdline.txt"), &vendor_boot.cmdline)?;
    fs::write(workdir.join("vendor_bootconfig.txt"), &vendor_boot.bootconfig)?;
    // Compressed ramdisks written by older runner versions.
    for stale in ["init_ramdisk.bin", "vendor_ramdisk.bin"] {
        let _ = fs::remove_file(workdir.join(stale));
    }
    println!(
        "[prepare] Extracted kernel ({:.1} MiB), init_ramdisk ({:.1} MiB), vendor_ramdisk ({:.1} MiB) [uncompressed cpio]",
        boot.kernel.len() as f64 / (1024.0 * 1024.0),
        init_cpio.len() as f64 / (1024.0 * 1024.0),
        vendor_boot.ramdisk.len() as f64 / (1024.0 * 1024.0),
    );
    Ok(())
}

fn cmd_prepare(images_dir: &Path, workdir: &Path, userdata_gb: u64) -> Result<()> {
    unpack_boot_images(images_dir, workdir)?;

    // Assemble Cuttlefish GPT disk.img.
    let boot_p = images_dir.join("boot.img");
    let init_boot_p = images_dir.join("init_boot.img");
    let vendor_boot_p = images_dir.join("vendor_boot.img");
    let super_p = images_dir.join("super.img");
    let vbmeta_p = images_dir.join("vbmeta.img");
    let vbmeta_sys_p = images_dir.join("vbmeta_system.img");
    let vbmeta_sys_dlkm_p = images_dir.join("vbmeta_system_dlkm.img");
    let vbmeta_vnd_dlkm_p = images_dir.join("vbmeta_vendor_dlkm.img");
    let userdata_p = images_dir.join("userdata.img");

    let mut parts: Vec<PartitionSpec<'_>> = vec![
        PartitionSpec {
            name: "misc",
            source: PartitionSource::Empty { size_bytes: 1024 * 1024 },
            min_size_bytes: 1024 * 1024,
        },
        // Factory reset protection (`ro.frp.pst`). PersistentDataBlockService
        // formats it on first boot; without it system_server crash-loops.
        PartitionSpec {
            name: "frp",
            source: PartitionSource::Empty { size_bytes: 1024 * 1024 },
            min_size_bytes: 1024 * 1024,
        },
        PartitionSpec {
            name: "boot_a",
            source: PartitionSource::File(&boot_p),
            min_size_bytes: 0,
        },
        PartitionSpec {
            name: "init_boot_a",
            source: PartitionSource::File(&init_boot_p),
            min_size_bytes: 0,
        },
        PartitionSpec {
            name: "vendor_boot_a",
            source: PartitionSource::File(&vendor_boot_p),
            min_size_bytes: 0,
        },
    ];

    for (name, path) in [
        ("vbmeta_a", &vbmeta_p),
        ("vbmeta_system_a", &vbmeta_sys_p),
        ("vbmeta_system_dlkm_a", &vbmeta_sys_dlkm_p),
        ("vbmeta_vendor_dlkm_a", &vbmeta_vnd_dlkm_p),
    ] {
        if path.is_file() {
            parts.push(PartitionSpec {
                name,
                source: PartitionSource::File(path),
                min_size_bytes: 1024 * 1024,
            });
        }
    }

    parts.push(PartitionSpec {
        name: "super",
        source: PartitionSource::File(&super_p),
        min_size_bytes: 0,
    });
    parts.push(PartitionSpec {
        name: "metadata",
        source: PartitionSource::Empty { size_bytes: 64 * 1024 * 1024 },
        min_size_bytes: 64 * 1024 * 1024,
    });

    let userdata_bytes = userdata_gb.max(2) * 1024 * 1024 * 1024;
    if userdata_p.is_file() {
        parts.push(PartitionSpec {
            name: "userdata",
            source: PartitionSource::File(&userdata_p),
            min_size_bytes: userdata_bytes,
        });
    } else {
        parts.push(PartitionSpec {
            name: "userdata",
            source: PartitionSource::Empty { size_bytes: userdata_bytes },
            min_size_bytes: userdata_bytes,
        });
    }

    parts.push(PartitionSpec {
        name: "custom",
        source: PartitionSource::Empty { size_bytes: 1024 * 1024 },
        min_size_bytes: 1024 * 1024,
    });

    gpt::build_gpt_disk(&workdir.join("disk.img"), &parts)?;
    if prepare_secure_env_share(images_dir, workdir, true)? {
        println!("[prepare] Staged Cuttlefish secure_env into {}/share", workdir.display());
    }
    println!("[prepare] Done. Run `./run.sh` (or `aosp14-runner run`).");
    Ok(())
}

fn cmd_doctor(workdir: &Path, images_dir: &Path) -> Result<()> {
    let mut problems = Vec::new();
    let yn = |b: bool| if b { "yes" } else { "NO" };

    let supported = vz::is_supported();
    println!("Virtualization.framework supported: {}", yn(supported));
    if !supported {
        problems.push("this Mac does not support Virtualization.framework".to_string());
    }
    match has_vz_entitlement() {
        Some(true) => println!("Binary has com.apple.security.virtualization: yes"),
        Some(false) => {
            println!("Binary has com.apple.security.virtualization: NO");
            problems.push("sign the binary: `make host`".into());
        }
        None => println!("Binary has com.apple.security.virtualization: unknown"),
    }

    let imgs_ok = required_images_present(images_dir);
    println!("AOSP 14 Cuttlefish images ({}): {}", images_dir.display(), yn(imgs_ok));

    let shim = find_shim_binary(None);
    println!(
        "In-guest shim: {}",
        shim.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "NO".into())
    );
    if shim.is_none() {
        problems.push("build the in-guest shim: `make shim`".into());
    }
    let secure_env = images_dir.join("secure_env/bin/secure_env").is_file()
        && images_dir.join("secure_env/lib64/libc_musl.so").is_file();
    println!("Cuttlefish secure_env ({}/secure_env): {}", images_dir.display(), yn(secure_env));
    if !secure_env {
        problems.push(format!(
            "copy Cuttlefish's arm64 musl secure_env (bin/ + lib64/) into {}/secure_env (KeyMint is required)",
            images_dir.display()
        ));
    }

    let prepared = boot_artifacts_present(workdir) && workdir.join("disk.img").is_file();
    println!("Prepared workdir ({}): {}", workdir.display(), yn(prepared));

    if !prepared && !imgs_ok {
        problems.push(format!(
            "copy boot.img, init_boot.img, vendor_boot.img, super.img, vbmeta*.img into {}",
            images_dir.display()
        ));
    }

    println!(
        "adb: {}",
        which("adb").map(|p| p.display().to_string()).unwrap_or_else(|| "not found".into())
    );

    if problems.is_empty() {
        println!("\nAll set: ./run.sh (or aosp14-runner run)");
    } else {
        println!("\nTo fix:");
        for p in &problems {
            println!("  - {p}");
        }
    }
    Ok(())
}

fn build_boot_props(a: &RunArgs, vendor_bootconfig: &str) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    for line in vendor_bootconfig.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = t.split_once('=') {
            let k = k.trim().to_string();
            let v = v.trim().trim_matches('"').to_string();
            map.insert(k, v);
        }
    }
    let pairs: Vec<(&str, String)> = vec![
        ("androidboot.slot_suffix", "_a".into()),
        ("androidboot.force_normal_boot", "1".into()),
        ("androidboot.verifiedbootstate", "orange".into()),
        ("androidboot.vbmeta.device_state", "unlocked".into()),
        ("androidboot.hardware", "cutf_cvm".into()),
        // Apple Virtualization.framework exposes VirtIO PCI devices under /devices/platform/40000000.pci
        ("androidboot.boot_devices", "40000000.pci".into()),
        ("androidboot.fstab_suffix", a.fstab_suffix.clone()),
        ("androidboot.console", "hvc0".into()),
        ("androidboot.serialconsole", "1".into()),
        ("androidboot.serialno", "AOSP14MAC0001".into()),
        ("androidboot.ddr_size", format!("{}MB", a.memory_mb)),
        ("androidboot.lcd_density", a.density.to_string()),
        ("androidboot.setupwizard_mode", "OPTIONAL".into()),
        ("androidboot.enable_bootanimation", "0".into()),
        ("androidboot.hw_timeout_multiplier", "3".into()),
        ("androidboot.hypervisor.vm.supported", "0".into()),
        ("androidboot.hypervisor.protected_vm.supported", "0".into()),
        ("androidboot.wifi_mac_prefix", "5554".into()),
        ("androidboot.vsock_lights_cid", VMADDR_CID_ANY.to_string()),
        ("androidboot.vsock_lights_port", LIGHTS_VSOCK_PORT.to_string()),
        // Cuttlefish `guest_swiftshader` graphics stack on 2D virtio-gpu DRM:
        // ANGLE (GLES 3.1) -> SwiftShader Vulkan (pastel) -> minigbm -> drm_hwcomposer/ranchu.
        ("androidboot.cpuvulkan.version", VK_API_VERSION_1_2.to_string()),
        ("androidboot.hardware.gralloc", "minigbm".into()),
        ("androidboot.hardware.hwcomposer", a.hwcomposer.clone()),
        ("androidboot.hardware.hwcomposer.display_finder_mode", "drm".into()),
        ("androidboot.hardware.egl", "angle".into()),
        ("androidboot.hardware.vulkan", "pastel".into()),
        ("androidboot.opengles.version", "196609".into()),
    ];
    for (k, v) in pairs {
        map.insert(k.to_string(), v);
    }
    if !a.enforcing {
        map.insert("androidboot.selinux".into(), "permissive".into());
    }
    if let Some(extra) = &a.extra_bootconfig {
        for kv in extra.split(';') {
            let t = kv.trim();
            if let Some((k, v)) = t.split_once('=') {
                map.insert(k.trim().to_string(), v.trim().trim_matches('"').to_string());
            }
        }
    }
    map
}

fn format_bootconfig(props: &std::collections::BTreeMap<String, String>) -> String {
    let mut s = String::new();
    for (k, v) in props {
        s.push_str(&format!("{k} = \"{v}\"\n"));
    }
    s
}

fn cmd_run(a: RunArgs) -> Result<()> {
    let kernel_path = a.workdir.join(KERNEL_FILE);
    let init_rd_path = a.workdir.join(INIT_RAMDISK_FILE);
    let vendor_rd_path = a.workdir.join(VENDOR_RAMDISK_FILE);
    let disk_path = a.workdir.join("disk.img");

    let need_disk = a.wipe_data || !disk_path.is_file();
    if !need_disk {
        let names = gpt::partition_names(&disk_path)?;
        if let Some(missing) = REQUIRED_PARTITIONS.iter().find(|p| !names.iter().any(|n| n == *p)) {
            bail!(
                "{} has no `{missing}` partition (created by an older aosp14-runner); \
                 re-run with --wipe-data to rebuild it (this resets userdata)",
                disk_path.display()
            );
        }
    }
    let need_boot = !boot_artifacts_present(&a.workdir);
    if need_disk || need_boot {
        if !required_images_present(&a.images_dir) {
            bail!(
                "workdir {} is not prepared and {} is missing required Cuttlefish images; run `prepare` first",
                a.workdir.display(),
                a.images_dir.display()
            );
        }
        if need_disk {
            println!("[run] Preparing workdir from {} ...", a.images_dir.display());
            cmd_prepare(&a.images_dir, &a.workdir, a.userdata_gb)?;
        } else {
            // Keep disk.img (userdata); only refresh kernel + ramdisks.
            unpack_boot_images(&a.images_dir, &a.workdir)?;
        }
    }

    let init_rd = fs::read(&init_rd_path).context("reading init ramdisk")?;
    let vendor_rd = fs::read(&vendor_rd_path).context("reading vendor ramdisk")?;
    let vendor_cmdline = fs::read_to_string(a.workdir.join("vendor_cmdline.txt")).unwrap_or_default();
    let vendor_bootconfig =
        fs::read_to_string(a.workdir.join("vendor_bootconfig.txt")).unwrap_or_default();

    let props = build_boot_props(&a, &vendor_bootconfig);
    let bootconfig = format_bootconfig(&props);

    // Both are hard requirements: Android 14 encrypts /data with keys from
    // KeyMint, which Cuttlefish provides through secure_env. Without it vold
    // (first boot) or keystore2 (every boot) blocks forever.
    let Some(shim) = find_shim_binary(a.shim_bin.as_deref()) else {
        bail!(
            "assets/aosp14-shim not found; build it with `make shim` (or pass --shim-bin). \
             The in-guest shim wires up the HAL consoles and runs secure_env."
        );
    };
    if !prepare_secure_env_share(&a.images_dir, &a.workdir, a.wipe_data)? {
        bail!(
            "{}/secure_env/bin/secure_env not found. Android 14 needs KeyMint to unlock /data; \
             copy Cuttlefish's arm64 musl secure_env (bin/secure_env plus its lib64/, including \
             libc_musl.so) into {}/secure_env/ (see README).",
            a.images_dir.display(),
            a.images_dir.display()
        );
    }
    let shim_bytes = fs::read(&shim).with_context(|| format!("reading shim {}", shim.display()))?;
    let shim_cpio = bootimg::build_single_file_cpio("aosp14-shim", 0o755, &shim_bytes);
    let share_dir = fs::canonicalize(a.workdir.join("share"))?;
    let init_arg = "rdinit=/aosp14-shim".to_string();
    println!(
        "[run] In-guest shim {} (consoles, Cuttlefish secure_env: KeyMint/Gatekeeper/OEMLock)",
        shim.display()
    );

    let initrd_bytes = bootimg::assemble_initrd_with_bootconfig(
        &vendor_rd,
        &init_rd,
        Some(&shim_cpio),
        &bootconfig,
    );
    let initrd_path = a.workdir.join("initrd.img");
    fs::write(&initrd_path, &initrd_bytes)?;

    let mut cmdline = vec![
        "bootconfig".to_string(),
        "console=hvc0".to_string(),
        "earlycon=hvc0".to_string(),
        "printk.devkmsg=on".to_string(),
        init_arg,
        "loop.max_part=7".to_string(),
        "firmware_class.path=/vendor/etc/".to_string(),
        "cma=0".to_string(),
    ];
    for tok in vendor_cmdline.split_whitespace() {
        if !cmdline.iter().any(|existing| existing == tok) {
            cmdline.push(tok.to_string());
        }
    }
    // Also mirror androidboot.* on /proc/cmdline in case VZ pads initrd_end past #BOOTCONFIG.
    for (k, v) in &props {
        if k.starts_with("androidboot.") {
            cmdline.push(format!("{k}={v}"));
        }
    }
    if let Some(extra) = &a.extra_cmdline {
        cmdline.push(extra.clone());
    }

    println!(
        "[run] Booting AOSP 14 VM (CPUs={}, RAM={}MiB, {}x{} @ {}dpi, hwc={}, audio={})",
        a.cpus, a.memory_mb, a.width, a.height, a.density, a.hwcomposer, !a.no_audio
    );
    println!(
        "[run] ADB: adb connect 127.0.0.1:{} (forwarded to adbd vsock:{})",
        a.adb_port, ADB_VSOCK_PORT
    );
    // Cuttlefish's `seriallogging` service streams `logcat -b all` to /dev/hvc2.
    let logcat_path = fs::canonicalize(&a.workdir)?.join("logcat.log");
    println!("[run] Guest logcat: {}", logcat_path.display());

    vz::run_vm(&vz::VmLaunchOptions {
        kernel: fs::canonicalize(&kernel_path)?,
        initrd: fs::canonicalize(&initrd_path)?,
        cmdline: cmdline.join(" "),
        disk: fs::canonicalize(&disk_path)?,
        share_dir: Some(share_dir),
        share_tag: Some("shared".to_string()),
        share_read_only: false,
        logcat_path: Some(logcat_path),
        cpus: a.cpus,
        memory_mb: a.memory_mb,
        width: a.width,
        height: a.height,
        adb_host_port: a.adb_port,
        adb_guest_vsock_port: ADB_VSOCK_PORT,
        enable_audio: !a.no_audio,
        headless: a.headless,
        serial_stdin: a.headless || a.serial_stdin,
    })
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Doctor { workdir, images_dir } => cmd_doctor(&workdir, &images_dir),
        Commands::Prepare { images_dir, workdir, userdata_gb } => {
            cmd_prepare(&images_dir, &workdir, userdata_gb)
        }
        Commands::Run(args) => cmd_run(args),
    }
}
