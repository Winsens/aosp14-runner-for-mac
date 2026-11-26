#import <AppKit/AppKit.h>
#import <Foundation/Foundation.h>
#import <Virtualization/Virtualization.h>
#import <objc/runtime.h>
#include "vz_bridge.h"

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

/* Seconds the guest gets to power off by itself before the VM is hard-stopped. */
static const int64_t kGracefulStopTimeout = 15;
/* Android's own shutdown sequence (sys.powerctl) can take a while. */
static const int64_t kGuestShutdownTimeout = 30;

/* aosp14-shim control channel in the guest (CONTROL_VSOCK_PORT in
 * guest/aosp14-shim/src/main.rs): accepts "shutdown\n", replies "ok\n". */
static const uint32_t kControlVsockPort = 6680;

/* Console devices for /dev/hvc0 .. hvc12: Cuttlefish (Android 14) assigns hvc
 * channels up to hvc12 (NFC). */
static const NSUInteger kConsolePortCount = 13;
/* Cuttlefish `seriallogging` writes `logcat -b all` to /dev/hvc2. */
static const NSUInteger kLogcatHvc = 2;

static NSString *ns_from_cstr(const char *s) {
    if (!s || s[0] == '\0') return nil;
    return [NSString stringWithUTF8String:s];
}

static VZVirtioSocketDevice *socket_device(VZVirtualMachine *vm) {
    for (VZSocketDevice *dev in vm.socketDevices) {
        if ([dev isKindOfClass:[VZVirtioSocketDevice class]]) {
            return (VZVirtioSocketDevice *)dev;
        }
    }
    return nil;
}

static bool write_all(int fd, const uint8_t *buf, ssize_t n) {
    ssize_t off = 0;
    while (off < n) {
        ssize_t w = write(fd, buf + off, (size_t)(n - off));
        if (w < 0 && errno == EINTR) continue;
        if (w <= 0) return false;
        off += w;
    }
    return true;
}

static void relay_tcp_vsock(int tcp_fd, VZVirtioSocketConnection *vsock_conn) {
    int vsock_fd = vsock_conn.fileDescriptor;
    if (vsock_fd < 0) {
        close(tcp_fd);
        [vsock_conn close];
        return;
    }

    int fds[2] = {tcp_fd, vsock_fd};
    bool open_in[2] = {true, true};
    uint8_t buf[65536];

    while (open_in[0] || open_in[1]) {
        struct pollfd pfd[2];
        nfds_t n = 0;
        int which[2];
        for (int i = 0; i < 2; i++) {
            if (!open_in[i]) continue;
            pfd[n].fd = fds[i];
            pfd[n].events = POLLIN;
            pfd[n].revents = 0;
            which[n] = i;
            n++;
        }
        int rc = poll(pfd, n, -1);
        if (rc < 0) {
            if (errno == EINTR) continue;
            break;
        }
        bool failed = false;
        for (nfds_t k = 0; k < n && !failed; k++) {
            if (!(pfd[k].revents & (POLLIN | POLLHUP | POLLERR | POLLNVAL))) continue;
            int src = which[k];
            int dst = 1 - src;
            ssize_t r = read(fds[src], buf, sizeof(buf));
            if (r < 0 && errno == EINTR) continue;
            if (r <= 0) {
                open_in[src] = false;
                shutdown(fds[dst], SHUT_WR);
                if (r < 0) failed = true;
                continue;
            }
            if (!write_all(fds[dst], buf, r)) failed = true;
        }
        if (failed) break;
    }

    close(tcp_fd);
    [vsock_conn close];
}

static void start_adb_vsock_forwarder(VZVirtualMachine *vm,
                                      dispatch_queue_t vm_queue,
                                      uint16_t host_port,
                                      uint32_t guest_vsock_port) {
    if (host_port == 0 || guest_vsock_port == 0) return;

    int listen_fd = socket(AF_INET, SOCK_STREAM, 0);
    if (listen_fd < 0) {
        fprintf(stderr, "[adb-fwd] socket() failed: %s\n", strerror(errno));
        return;
    }

    int yes = 1;
    setsockopt(listen_fd, SOL_SOCKET, SO_REUSEADDR, &yes, sizeof(yes));
    setsockopt(listen_fd, SOL_SOCKET, SO_REUSEPORT, &yes, sizeof(yes));

    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    addr.sin_port = htons(host_port);

    if (bind(listen_fd, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
        fprintf(stderr, "[adb-fwd] bind(127.0.0.1:%u) failed: %s\n", host_port, strerror(errno));
        close(listen_fd);
        return;
    }
    if (listen(listen_fd, 32) < 0) {
        fprintf(stderr, "[adb-fwd] listen() failed: %s\n", strerror(errno));
        close(listen_fd);
        return;
    }

    fprintf(stderr, "[adb-fwd] Forwarding 127.0.0.1:%u -> guest vsock:%u\n",
            host_port, guest_vsock_port);

    dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0), ^{
        while (1) {
            int client_fd = accept(listen_fd, NULL, NULL);
            if (client_fd < 0) {
                if (errno == EINTR) continue;
                break;
            }
            int one = 1;
            setsockopt(client_fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));
            dispatch_async(vm_queue, ^{
                VZVirtioSocketDevice *sock_dev = socket_device(vm);
                if (!sock_dev) {
                    close(client_fd);
                    return;
                }
                [sock_dev connectToPort:guest_vsock_port
                      completionHandler:^(VZVirtioSocketConnection *conn, NSError *err) {
                    if (err || !conn) {
                        close(client_fd);
                        return;
                    }
                    dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0), ^{
                        relay_tcp_vsock(client_fd, conn);
                    });
                }];
            });
        }
        close(listen_fd);
    });
}

@interface RunnerAppDelegate : NSObject <NSApplicationDelegate, NSWindowDelegate, VZVirtualMachineDelegate>
@property (nonatomic, strong) VZVirtualMachine *vm;
@property (nonatomic, strong) dispatch_queue_t vmQueue;
@property (nonatomic, strong) NSWindow *window;
@property (nonatomic, strong) VZVirtualMachineView *vmView;
@property (nonatomic, assign) uint32_t displayWidth;
@property (nonatomic, assign) uint32_t displayHeight;
@property (nonatomic, assign) uint16_t adbHostPort;
@property (nonatomic, assign) uint32_t adbGuestPort;
@property (nonatomic, assign) bool headless;
@property (nonatomic, assign) BOOL stopping;
@property (nonatomic, assign) BOOL finished;
@property (nonatomic, assign) int exitCode;
@property (nonatomic, strong) NSMutableArray *signalSources;
@end

@implementation RunnerAppDelegate

- (void)finishWithCode:(int)code {
    dispatch_async(dispatch_get_main_queue(), ^{
        if (self.finished) return;
        self.finished = YES;
        self.exitCode = code;
        [NSApp stop:nil];
        NSEvent *wake = [NSEvent otherEventWithType:NSEventTypeApplicationDefined
                                           location:NSZeroPoint
                                      modifierFlags:0
                                          timestamp:0
                                       windowNumber:0
                                            context:nil
                                            subtype:0
                                              data1:0
                                              data2:0];
        [NSApp postEvent:wake atStart:YES];
    });
}

- (void)hardStop {
    dispatch_async(self.vmQueue, ^{
        if (self.vm.canStop) {
            [self.vm stopWithCompletionHandler:^(NSError *err) {
                if (err) {
                    fprintf(stderr, "[vz] stop failed: %s\n", err.localizedDescription.UTF8String);
                }
                [self finishWithCode:0];
            }];
        } else {
            [self finishWithCode:0];
        }
    });
}

/* Must be called on vmQueue. */
- (void)scheduleHardStopAfter:(int64_t)seconds {
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, seconds * NSEC_PER_SEC), self.vmQueue, ^{
        if (!self.finished) {
            fprintf(stderr, "[vz] Guest did not power off in %llds; stopping VM.\n", (long long)seconds);
            [self hardStop];
        }
    });
}

/* Fallback: VZ's stop request arrives as a power-key press, which Android
 * normally treats as "screen off". Must be called on vmQueue. */
- (void)requestStopViaPowerKey {
    NSError *err = nil;
    if (self.vm.canRequestStop && [self.vm requestStopWithError:&err]) {
        fprintf(stderr, "[vz] Sent power-key stop request (press Ctrl-C again to force)...\n");
        [self scheduleHardStopAfter:kGracefulStopTimeout];
    } else {
        [self hardStop];
    }
}

- (void)requestStop {
    if (self.stopping) {
        fprintf(stderr, "[vz] Forcing VM stop.\n");
        [self hardStop];
        return;
    }
    self.stopping = YES;
    dispatch_async(self.vmQueue, ^{
        if (self.vm.state != VZVirtualMachineStateRunning) {
            [self hardStop];
            return;
        }
        VZVirtioSocketDevice *dev = socket_device(self.vm);
        if (!dev) {
            [self requestStopViaPowerKey];
            return;
        }
        // Preferred: ask Android to power off via the aosp14-shim control channel.
        [dev connectToPort:kControlVsockPort
         completionHandler:^(VZVirtioSocketConnection *conn, NSError *err) {
            if (err || !conn) {
                dispatch_async(self.vmQueue, ^{ [self requestStopViaPowerKey]; });
                return;
            }
            dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0), ^{
                bool ok = false;
                int fd = conn.fileDescriptor;
                static const char cmd[] = "shutdown\n";
                if (fd >= 0 && write_all(fd, (const uint8_t *)cmd, sizeof(cmd) - 1)) {
                    struct pollfd pfd = { .fd = fd, .events = POLLIN };
                    char reply[16] = {0};
                    if (poll(&pfd, 1, 3000) > 0 && read(fd, reply, sizeof(reply) - 1) > 0) {
                        ok = strncmp(reply, "ok", 2) == 0;
                    }
                }
                [conn close];
                dispatch_async(self.vmQueue, ^{
                    if (ok) {
                        fprintf(stderr, "[vz] Android is shutting down (press Ctrl-C again to force)...\n");
                        [self scheduleHardStopAfter:kGuestShutdownTimeout];
                    } else {
                        [self requestStopViaPowerKey];
                    }
                });
            });
        }];
    });
}

- (void)installMenu {
    NSMenu *mainMenu = [[NSMenu alloc] init];
    NSMenuItem *appItem = [[NSMenuItem alloc] init];
    [mainMenu addItem:appItem];
    NSMenu *appMenu = [[NSMenu alloc] init];
    [appMenu addItemWithTitle:@"Quit aosp14-runner"
                       action:@selector(terminate:)
                keyEquivalent:@"q"];
    appItem.submenu = appMenu;
    NSApp.mainMenu = mainMenu;
}

- (void)installSignalHandlers {
    self.signalSources = [NSMutableArray array];
    for (int sig = SIGINT, i = 0; i < 2; i++, sig = SIGTERM) {
        signal(sig, SIG_IGN);
        dispatch_source_t src =
            dispatch_source_create(DISPATCH_SOURCE_TYPE_SIGNAL, (uintptr_t)sig, 0, dispatch_get_main_queue());
        dispatch_source_set_event_handler(src, ^{
            fprintf(stderr, "\n");
            [self requestStop];
        });
        dispatch_resume(src);
        [self.signalSources addObject:src];
    }
}

- (void)applicationDidFinishLaunching:(NSNotification *)notification {
    [self installSignalHandlers];
    if (!self.headless) {
        [self installMenu];
        CGFloat winW = MAX(360.0, (CGFloat)self.displayWidth / 2.0);
        CGFloat winH = MAX(640.0, (CGFloat)self.displayHeight / 2.0);

        NSRect frame = NSMakeRect(120, 120, winW, winH);
        NSWindowStyleMask style = NSWindowStyleMaskTitled |
                                  NSWindowStyleMaskClosable |
                                  NSWindowStyleMaskMiniaturizable |
                                  NSWindowStyleMaskResizable;
        self.window = [[NSWindow alloc] initWithContentRect:frame
                                                  styleMask:style
                                                    backing:NSBackingStoreBuffered
                                                      defer:NO];
        self.window.title = @"AOSP 14 (ARM64)";
        self.window.delegate = self;
        self.window.contentAspectRatio = NSMakeSize(winW, winH);

        self.vmView = [[VZVirtualMachineView alloc] initWithFrame:NSMakeRect(0, 0, winW, winH)];
        self.vmView.capturesSystemKeys = NO;
        if (@available(macOS 14.0, *)) {
            self.vmView.automaticallyReconfiguresDisplay = NO;
        }
        self.vmView.virtualMachine = self.vm;
        [self.window setContentView:self.vmView];
        [self.window makeKeyAndOrderFront:nil];
        [self.window makeFirstResponder:self.vmView];
        [NSApp activateIgnoringOtherApps:YES];
    }

    dispatch_async(self.vmQueue, ^{
        [self.vm startWithCompletionHandler:^(NSError *error) {
            if (error) {
                fprintf(stderr, "[vz] Failed to start VM: %s\n",
                        error.localizedDescription.UTF8String);
                [self finishWithCode:1];
                return;
            }
            fprintf(stderr, "[vz] VM started successfully.\n");
            start_adb_vsock_forwarder(self.vm, self.vmQueue, self.adbHostPort, self.adbGuestPort);
        }];
    });
}

- (NSApplicationTerminateReply)applicationShouldTerminate:(NSApplication *)sender {
    [self requestStop];
    return NSTerminateCancel;
}

- (BOOL)windowShouldClose:(NSWindow *)sender {
    [self requestStop];
    return NO;
}

- (void)guestDidStopVirtualMachine:(VZVirtualMachine *)virtualMachine {
    fprintf(stderr, "[vz] Guest stopped.\n");
    [self finishWithCode:0];
}

- (void)virtualMachine:(VZVirtualMachine *)virtualMachine didStopWithError:(NSError *)error {
    fprintf(stderr, "[vz] VM stopped with error: %s\n",
            error.localizedDescription.UTF8String);
    [self finishWithCode:1];
}

@end

bool vz_is_supported(void) {
    return [VZVirtualMachine isSupported];
}

int vz_run_vm(const VzVmConfig *cfg) {
    if (!cfg) return 1;
    if (![VZVirtualMachine isSupported]) {
        fprintf(stderr, "[vz] Virtualization.framework is not supported on this host.\n");
        return 1;
    }

    @autoreleasepool {
      @try {
        NSString *kernelPath = ns_from_cstr(cfg->kernel_path);
        NSString *initrdPath = ns_from_cstr(cfg->initrd_path);
        NSString *cmdline = ns_from_cstr(cfg->cmdline);
        NSString *diskPath = ns_from_cstr(cfg->disk_path);
        NSString *shareDir = ns_from_cstr(cfg->share_dir);
        NSString *shareTag = ns_from_cstr(cfg->share_tag);

        if (!kernelPath || !initrdPath) {
            fprintf(stderr, "[vz] kernel_path and initrd_path are required.\n");
            return 1;
        }

        VZVirtualMachineConfiguration *vmConfig = [[VZVirtualMachineConfiguration alloc] init];
        vmConfig.CPUCount = cfg->cpu_count > 0 ? cfg->cpu_count : 4;
        vmConfig.memorySize = (uint64_t)(cfg->memory_mb > 0 ? cfg->memory_mb : 4096) * 1024ULL * 1024ULL;

        VZLinuxBootLoader *bootLoader = [[VZLinuxBootLoader alloc]
            initWithKernelURL:[NSURL fileURLWithPath:kernelPath]];
        bootLoader.initialRamdiskURL = [NSURL fileURLWithPath:initrdPath];
        if (cmdline) {
            bootLoader.commandLine = cmdline;
        }
        vmConfig.bootLoader = bootLoader;
        vmConfig.platform = [[VZGenericPlatformConfiguration alloc] init];
        vmConfig.entropyDevices = @[ [[VZVirtioEntropyDeviceConfiguration alloc] init] ];

        // VirtIO consoles for the Cuttlefish HALs:
        //   consoleDevices[N] -> /dev/hvcN (Cuttlefish numbering): one
        //   virtio-console device per hvc, each with a single console port named
        //   "cf-hvcN". aosp14-shim binds them one at a time in PCI order, so hvc
        //   numbers are stable. (Console ports of ONE multiport device get hvc
        //   numbers in the order Virtualization.framework announces them, a hash
        //   order (observed 0,5,1,12,2,13,...) that silently cross-wires the HALs;
        //   single-port serialPorts would be ordered, but VZ allows only 10.)
        //     hvc0  : host console (stdout / optional stdin)
        //     hvc2  : logcat (seriallogging) -> cfg->logcat_path
        //     hvc4  : Gatekeeper HAL  <--> consoleDev port "se_gatekeeper"
        //     hvc10 : OEMLock HAL     <--> consoleDev port "se_oemlock"
        //     hvc11 : KeyMint Rust    <--> consoleDev port "se_keymint"
        //     other : sink (/dev/null write + idle pipe read). HALs such as NFC
        //             (hvc12) only need the device to exist; without it they
        //             crash-loop on open() with ENODEV.
        //   consoleDev (multiport, isConsole = NO -> /dev/vport*p*, raw streams):
        //     named ports "se_gatekeeper", "se_oemlock", "se_keymint" (found by
        //     name, so order does not matter) cross-wired via host pipes to
        //     hvc4, hvc10, hvc11 (zero relay threads).
        if (@available(macOS 13.0, *)) {
            int devNullFd = open("/dev/null", O_WRONLY);
            int idlePipe[2] = {-1, -1};
            if (pipe(idlePipe) != 0) {
                fprintf(stderr, "[vz] pipe() failed: %s\n", strerror(errno));
                return 1;
            }
            // Retain write-end handle in NSArray so VZ never sees EOF on idle read ports.
            NSMutableArray<NSFileHandle *> *retainedHandles = [NSMutableArray array];
            [retainedHandles addObject:[[NSFileHandle alloc] initWithFileDescriptor:idlePipe[1] closeOnDealloc:YES]];

            NSMutableArray<VZSerialPortAttachment *> *hvc = [NSMutableArray array];
            VZVirtioConsoleDeviceConfiguration *consoleDev = [[VZVirtioConsoleDeviceConfiguration alloc] init];

            // /dev/hvc0: kernel & serial console -> host stdout / optional stdin.
            int consolePipe[2];
            if (pipe(consolePipe) != 0) {
                fprintf(stderr, "[vz] pipe() failed: %s\n", strerror(errno));
                return 1;
            }
            NSFileHandle *readHandle = [[NSFileHandle alloc] initWithFileDescriptor:consolePipe[0] closeOnDealloc:YES];
            NSFileHandle *writeHandle = [[NSFileHandle alloc] initWithFileDescriptor:consolePipe[1] closeOnDealloc:YES];
            dispatch_async(dispatch_get_global_queue(QOS_CLASS_UTILITY, 0), ^{
                while (1) {
                    NSData *data = [readHandle availableData];
                    if (data.length == 0) break;
                    fwrite(data.bytes, 1, data.length, stdout);
                    fflush(stdout);
                }
            });
            NSFileHandle *stdinHandle = cfg->serial_stdin
                ? [NSFileHandle fileHandleWithStandardInput]
                : nil;
            [hvc addObject:[[VZFileHandleSerialPortAttachment alloc]
                               initWithFileHandleForReading:stdinHandle
                                       fileHandleForWriting:writeHandle]];

            // Create 3 bidirectional loopback pipe pairs for secure_env channels.
            struct {
                NSUInteger hvcIdx;
                NSUInteger vportIdx;
                NSString *name;
            } loopbacks[3] = {
                { 4,  0, @"se_gatekeeper" },
                { 10, 1, @"se_oemlock" },
                { 11, 2, @"se_keymint" },
            };

            for (NSUInteger i = 1; i < kConsolePortCount; i++) {
                int outFd = -1;
                if (i == kLogcatHvc && cfg->logcat_path && cfg->logcat_path[0]) {
                    outFd = open(cfg->logcat_path, O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0644);
                    if (outFd < 0) {
                        fprintf(stderr, "[vz] cannot open %s: %s\n", cfg->logcat_path, strerror(errno));
                    }
                }
                if (outFd < 0) outFd = dup(devNullFd);
                NSFileHandle *rIdle = [[NSFileHandle alloc] initWithFileDescriptor:dup(idlePipe[0]) closeOnDealloc:YES];
                NSFileHandle *wNull = [[NSFileHandle alloc] initWithFileDescriptor:outFd closeOnDealloc:YES];
                [hvc addObject:[[VZFileHandleSerialPortAttachment alloc]
                                   initWithFileHandleForReading:rIdle
                                           fileHandleForWriting:wNull]];
            }

            for (int k = 0; k < 3; k++) {
                int guestToSe[2] = {-1, -1};
                int seToGuest[2] = {-1, -1};
                if (pipe(guestToSe) != 0 || pipe(seToGuest) != 0) {
                    fprintf(stderr, "[vz] loopback pipe() failed: %s\n", strerror(errno));
                    return 1;
                }
                NSFileHandle *gRead  = [[NSFileHandle alloc] initWithFileDescriptor:seToGuest[0] closeOnDealloc:YES];
                NSFileHandle *gWrite = [[NSFileHandle alloc] initWithFileDescriptor:guestToSe[1] closeOnDealloc:YES];
                NSFileHandle *sRead  = [[NSFileHandle alloc] initWithFileDescriptor:guestToSe[0] closeOnDealloc:YES];
                NSFileHandle *sWrite = [[NSFileHandle alloc] initWithFileDescriptor:seToGuest[1] closeOnDealloc:YES];

                // Replaces the sink created above for this hvc index.
                hvc[loopbacks[k].hvcIdx] = [[VZFileHandleSerialPortAttachment alloc]
                    initWithFileHandleForReading:gRead
                            fileHandleForWriting:gWrite];

                VZVirtioConsolePortConfiguration *sePort = [[VZVirtioConsolePortConfiguration alloc] init];
                sePort.isConsole = NO;
                sePort.name = loopbacks[k].name;
                sePort.attachment = [[VZFileHandleSerialPortAttachment alloc]
                    initWithFileHandleForReading:sRead
                            fileHandleForWriting:sWrite];
                consoleDev.ports[loopbacks[k].vportIdx] = sePort;
            }

            // hvc devices first (in hvc order), the named secure_env device last.
            NSMutableArray<VZConsoleDeviceConfiguration *> *consoles = [NSMutableArray array];
            for (NSUInteger i = 0; i < hvc.count; i++) {
                VZVirtioConsolePortConfiguration *port = [[VZVirtioConsolePortConfiguration alloc] init];
                port.isConsole = YES;
                port.name = [NSString stringWithFormat:@"cf-hvc%lu", (unsigned long)i];
                port.attachment = hvc[i];
                VZVirtioConsoleDeviceConfiguration *dev = [[VZVirtioConsoleDeviceConfiguration alloc] init];
                dev.ports[0] = port;
                [consoles addObject:dev];
            }
            [consoles addObject:consoleDev];

            close(devNullFd);
            close(idlePipe[0]);
            // Keep retainedHandles alive for the lifetime of the VM process.
            objc_setAssociatedObject(vmConfig, "aosp14RetainedHandles", retainedHandles, OBJC_ASSOCIATION_RETAIN_NONATOMIC);
            vmConfig.consoleDevices = consoles;
        } else {
            fprintf(stderr, "[vz] macOS 13.0+ is required for multi-port VirtIO console.\n");
            return 1;
        }

        VZVirtioNetworkDeviceConfiguration *netCfg = [[VZVirtioNetworkDeviceConfiguration alloc] init];
        netCfg.attachment = [[VZNATNetworkDeviceAttachment alloc] init];
        vmConfig.networkDevices = @[ netCfg ];

        NSMutableArray<VZStorageDeviceConfiguration *> *storageDevs = [NSMutableArray array];
        if (diskPath) {
            NSError *diskErr = nil;
            VZDiskImageStorageDeviceAttachment *diskAttach =
                [[VZDiskImageStorageDeviceAttachment alloc]
                    initWithURL:[NSURL fileURLWithPath:diskPath]
                       readOnly:NO
                          error:&diskErr];
            if (!diskAttach || diskErr) {
                fprintf(stderr, "[vz] Failed to attach disk %s: %s\n",
                        diskPath.UTF8String,
                        diskErr ? diskErr.localizedDescription.UTF8String : "unknown");
                return 1;
            }
            [storageDevs addObject:[[VZVirtioBlockDeviceConfiguration alloc] initWithAttachment:diskAttach]];
        }
        vmConfig.storageDevices = storageDevs;

        if (shareDir && shareTag) {
            NSError *tagErr = nil;
            if (![VZVirtioFileSystemDeviceConfiguration validateTag:shareTag error:&tagErr]) {
                fprintf(stderr, "[vz] Invalid share tag: %s\n", tagErr.localizedDescription.UTF8String);
                return 1;
            }
            VZSharedDirectory *shared = [[VZSharedDirectory alloc]
                initWithURL:[NSURL fileURLWithPath:shareDir] readOnly:(cfg->share_read_only ? YES : NO)];
            VZSingleDirectoryShare *singleShare = [[VZSingleDirectoryShare alloc] initWithDirectory:shared];
            VZVirtioFileSystemDeviceConfiguration *fsCfg =
                [[VZVirtioFileSystemDeviceConfiguration alloc] initWithTag:shareTag];
            fsCfg.share = singleShare;
            vmConfig.directorySharingDevices = @[ fsCfg ];
        }

        uint32_t w = cfg->display_width > 0 ? cfg->display_width : 720;
        uint32_t h = cfg->display_height > 0 ? cfg->display_height : 1280;
        VZVirtioGraphicsDeviceConfiguration *gfxCfg = [[VZVirtioGraphicsDeviceConfiguration alloc] init];
        VZVirtioGraphicsScanoutConfiguration *scanout =
            [[VZVirtioGraphicsScanoutConfiguration alloc] initWithWidthInPixels:w heightInPixels:h];
        gfxCfg.scanouts = @[ scanout ];
        vmConfig.graphicsDevices = @[ gfxCfg ];

        if (cfg->enable_audio) {
            if (@available(macOS 12.0, *)) {
                VZVirtioSoundDeviceConfiguration *soundCfg = [[VZVirtioSoundDeviceConfiguration alloc] init];
                VZVirtioSoundDeviceOutputStreamConfiguration *outStream =
                    [[VZVirtioSoundDeviceOutputStreamConfiguration alloc] init];
                outStream.sink = [[VZHostAudioOutputStreamSink alloc] init];
                soundCfg.streams = @[ outStream ];
                vmConfig.audioDevices = @[ soundCfg ];
            }
        }

        vmConfig.keyboards = @[ [[VZUSBKeyboardConfiguration alloc] init] ];
        vmConfig.pointingDevices = @[ [[VZUSBScreenCoordinatePointingDeviceConfiguration alloc] init] ];
        vmConfig.socketDevices = @[ [[VZVirtioSocketDeviceConfiguration alloc] init] ];

        NSError *validateErr = nil;
        if (![vmConfig validateWithError:&validateErr]) {
            fprintf(stderr, "[vz] VM configuration invalid: %s\n",
                    validateErr ? validateErr.localizedDescription.UTF8String : "unknown");
            return 1;
        }

        dispatch_queue_t vmQueue = dispatch_queue_create("com.aosp14.runner.vm", DISPATCH_QUEUE_SERIAL);
        VZVirtualMachine *vm = [[VZVirtualMachine alloc] initWithConfiguration:vmConfig queue:vmQueue];

        NSApplication *app = [NSApplication sharedApplication];
        [app setActivationPolicy:cfg->headless ? NSApplicationActivationPolicyProhibited
                                               : NSApplicationActivationPolicyRegular];

        RunnerAppDelegate *delegate = [[RunnerAppDelegate alloc] init];
        delegate.vm = vm;
        delegate.vmQueue = vmQueue;
        delegate.displayWidth = w;
        delegate.displayHeight = h;
        delegate.adbHostPort = cfg->adb_host_port;
        delegate.adbGuestPort = cfg->adb_guest_vsock_port;
        delegate.headless = cfg->headless;
        vm.delegate = delegate;
        app.delegate = delegate;

        [app run];
        return delegate.exitCode;
      } @catch (NSException *ex) {
        fprintf(stderr, "[vz] NSException %s: %s\n%s\n",
                ex.name.UTF8String,
                ex.reason ? ex.reason.UTF8String : "(null)",
                ex.callStackSymbols.description.UTF8String);
        return 1;
      }
    }
}
