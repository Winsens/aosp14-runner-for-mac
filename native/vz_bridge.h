#ifndef VZ_BRIDGE_H
#define VZ_BRIDGE_H

#include <stdint.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Must match `VzVmConfig` in src/vz.rs field for field. */
typedef struct {
    const char *kernel_path;
    const char *initrd_path;
    const char *cmdline;
    const char *disk_path;
    const char *share_dir;
    const char *share_tag;
    bool share_read_only;
    /* Optional host file for the guest's /dev/hvc2 (Cuttlefish logcat). */
    const char *logcat_path;
    uint32_t cpu_count;
    uint64_t memory_mb;
    uint32_t display_width;
    uint32_t display_height;
    uint16_t adb_host_port;
    uint32_t adb_guest_vsock_port;
    bool enable_audio;
    bool headless;
    /* Connect stdin to the guest serial console. */
    bool serial_stdin;
} VzVmConfig;

/// Check whether Apple Virtualization.framework is supported on this Mac.
bool vz_is_supported(void);

/// Boot the VM and run the macOS event loop (blocks until the VM stops or the
/// window is closed). Returns 0 when the guest powered off / was shut down,
/// non-zero on startup or runtime errors.
int vz_run_vm(const VzVmConfig *cfg);

#ifdef __cplusplus
}
#endif

#endif /* VZ_BRIDGE_H */
