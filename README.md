# aosp14-runner

Boots **unmodified** AOSP 14 (`aosp_cf_arm64_only_phone-ap2a-userdebug`) images natively on Apple Silicon macOS using Apple's `Virtualization.framework` — no QEMU, no Docker Desktop, no AOSP source patches, and no custom Linux kernel build required.

A fresh install reaches the launcher in about 17 s, later boots in about 13 s (M1, 4 vCPUs, 4 GiB).

## Architecture

```
macOS host (aosp14-runner)                      Android 14 GKI 6.1 guest (Cuttlefish arm64-only)
──────────────────────────                      ────────────────────────────────────────────────
images/                                         kernel-6.1 (from boot.img)
  boot.img, init_boot.img, vendor_boot.img ──▶  initrd.img: aosp14-shim + vendor + generic ramdisks
                                                  (decompressed newc cpio) + #BOOTCONFIG
  super.img, vbmeta*.img, userdata.img     ──▶  GPT disk.img (virtio-blk @ 40000000.pci)
                                                  ├─ misc, frp, boot/init_boot/vendor_boot_a
                                                  ├─ super (system, system_ext, product, vendor, *_dlkm)
                                                  ├─ vbmeta*_a (AVB)
                                                  └─ metadata & userdata (formatted on first boot)
  secure_env/ (arm64 musl) ── virtiofs ────▶  /mnt/vendor/shared ── secure_env (KeyMint/Gatekeeper/OEMLock)
                                                  ▲ named virtio ports se_* ◀─host pipes─▶ hvc4/hvc10/hvc11 HALs
stdout/stdin ── hvc0 (console) ──────────────▶  kernel log + Android serial console
workdir/logcat.log ◀── hvc2 ──────────────────  logcat -b all (Cuttlefish seriallogging)
127.0.0.1:15555 ── virtio-vsock:5555 ────────▶  adbd (listens on tcp:5555 and vsock:5555)
Ctrl-C / window close ── vsock:6680 ─────────▶  aosp14-shim control channel ─▶ sys.powerctl=shutdown
window ── virtio-gpu (DRM) + USB kbd/ptr ────▶  ANGLE (GLES 3.1) + SwiftShader Vulkan + minigbm + HWC
audio  ── virtio-snd ────────────────────────▶  virtio_snd.ko
net    ── VZ NAT (DHCP) ─────────────────────▶  eth0 (Android Ethernet)
```

## 1. Build unmodified AOSP 14 (Linux build host)

Zero source patches needed:

```sh
cd /path/to/android-14.0.0_r75
source build/envsetup.sh
lunch aosp_cf_arm64_only_phone-ap2a-userdebug
m -j$(nproc) droid
```

Copy the following files from `$OUT` (`out/target/product/vsoc_arm64_only/`) into `./images/` on your Mac:
- `boot.img`
- `init_boot.img`
- `vendor_boot.img`
- `super.img`
- `vbmeta.img`, `vbmeta_system.img`, `vbmeta_system_dlkm.img`, `vbmeta_vendor_dlkm.img`
- `userdata.img` (optional — runner creates a sparse empty userdata partition if omitted)

### Cuttlefish `secure_env` (required)

Android 14 encrypts `/data` with keys from KeyMint. On Cuttlefish, KeyMint, Gatekeeper and OEMLock are served by the host tool `secure_env`; the runner runs it **inside the guest**, so it needs an **arm64 Linux musl** build of it (as found in an arm64 musl Cuttlefish host build, `out/host/linux_musl-arm64/`):

```
images/secure_env/bin/secure_env
images/secure_env/lib64/*.so        # every library it links, including libc_musl.so
```

`libc_musl.so` doubles as the dynamic loader. Without `secure_env`, first boot hangs forever in vold, so `run` refuses to start and `doctor` reports it.

## 2. Build & run on macOS (Apple Silicon)

```sh
make                                   # in-guest shim (assets/aosp14-shim) + signed host runner
./target/release/aosp14-runner doctor
./run.sh                               # window; or ./run.sh --headless
```

`make shim` needs `rustup` (it adds the `aarch64-unknown-linux-musl` target and links with the bundled `rust-lld`, no cross compiler needed).

On first launch `run` unpacks `boot.img` / `init_boot.img` / `vendor_boot.img`, builds the sparse GPT `workdir/disk.img` and stages `secure_env` in `workdir/share`; later launches reuse them. `./run.sh` connects `adb` to `127.0.0.1:15555` once Android is up.

- **Stop:** close the window, Cmd-Q or Ctrl-C. Android powers off cleanly (a second Ctrl-C forces it). `adb reboot` reboots the guest in place.
- **Factory reset:** `--wipe-data` rebuilds `disk.img` (and resets the KeyMint TPM state).
- **Options:** `aosp14-runner run --help` (CPUs, memory, resolution, density, ports, extra kernel cmdline / bootconfig, ...).

## Logs and debugging

| Where | What |
| --- | --- |
| terminal (stdout) | kernel log from the very first line (hvc0), including init's messages |
| `workdir/logcat.log` | `logcat -b all`, written by Android's `seriallogging` service |
| `workdir/share/cf/instances/cvd-1/logs/secure_env.log` | secure_env (KeyMint TA) |
| `--headless` / `--serial-stdin` | stdin is the Android serial console (`console:/ $` shell on hvc0); works even when adbd is not up yet |

Lines prefixed `aosp14-shim:` in the kernel log come from the in-guest shim.

## How it runs stock Cuttlefish images

Cuttlefish normally runs under crosvm with a set of host daemons. The runner replaces what Android 14 needs from them; everything else boots as built.

- **Initramfs:** vendor ramdisk fragments and `init_boot`'s ramdisk are decompressed on the host and concatenated as plain cpio archives. The kernel's in-place lz4 decoder stops at the first byte it can't parse (padding between fragments, a raw cpio after lz4), silently dropping everything after it, including the generic `/init`. Recovery fragments are skipped and `androidboot.force_normal_boot=1` is set.
- **`aosp14-shim` (`rdinit`, `guest/aosp14-shim`):** a static musl binary that runs as PID 1 for a few hundred milliseconds and then `exec`s the untouched `/init`.
  - **Early console:** GKI builds virtio-pci/virtio-console as vendor modules and VZ has no UART, so the shim loads them itself; otherwise nothing is visible until (or if) first-stage init gets there.
  - **Stable `/dev/hvcN`:** the Cuttlefish HALs hardcode hvc numbers (KeyMint hvc11, Gatekeeper hvc4, OEMLock hvc10, logcat hvc2, NFC hvc12). Console ports of one VZ multiport device are announced in hash order, and VZ allows only 10 single-port serial devices, so the host creates one console device per hvc (port names `cf-hvcN`) and the shim binds them one at a time in PCI order. It logs `hvc numbering verified`, or a warning if VZ ever changes the order.
  - **secure_env:** the shim opens the three named `se_*` ports while still PID 1 (virtio-console discards data for ports nobody has open, which would lose KeyMint's first request), then runs `secure_env` from the virtiofs share once Android has mounted it.
  - **Network:** Cuttlefish renames `eth0` and expects Wi-Fi through a host-side wmediumd/OpenWRT access point. The shim sets `ro.vendor.disable_rename_eth0=1` before `post-fs-data`, so Android's Ethernet support runs DHCP against the VZ NAT.
  - **Shutdown:** a vsock control channel sets `sys.powerctl`. VZ's own stop request arrives as a power-key press, which Android treats as "screen off".
- **adb:** adbd listens on vsock:5555 as well as tcp:5555, so the host forwards there directly. Cuttlefish's `socket_vsock_proxy` (vsock:6520) is avoided: in android-14 it reads an uninitialized flag on every accepted connection and randomly aborts.
- **Bootconfig:** written as the initrd `#BOOTCONFIG` trailer and mirrored on the kernel command line. Besides the usual Cuttlefish values it sets `vsock_lights_cid/port`; unset, the lights HAL crashes and `system_server` blocks in `StartLightsService`.
- **Disk:** includes the `frp` partition (`ro.frp.pst`), without which `PersistentDataBlockService` makes `system_server` crash-loop. Disks from older runner versions lack it; `run` says so and asks for `--wipe-data`.

## Limitations

- **Bluetooth and NFC are switched off** after boot: their HALs talk to host emulators (rootcanal, casimir) that are not provided, and the framework stacks abort on the first unanswered command. On a fresh install each crashes once or twice before the switch-off.
- **No telephony modem, Wi-Fi, GNSS or UWB backends** (Cuttlefish host daemons). Networking is Ethernet.
- **SELinux is permissive** by default. The shim and `secure_env` run in the `kernel` domain, so `--enforcing` is experimental.

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).
Copyright 2026 Winsens.
