// simee's iOS camera shim, stage one: inserted into a process with DYLD_INSERT_LIBRARIES (per app
// through `simee launch`, or device-wide through `launchctl setenv` in the device). It links only
// libSystem, so a device-wide insert costs the device's daemons one small image and nothing else.
//
// It loads the hooks (libsimee-camera-hooks.dylib, beside this file) only into a user-installed
// app (an executable inside the device's Containers/Bundle/Application, not an app extension) when
// SIMEE_CAMERA_SOCKET names the feed. Apple's apps and daemons live in the runtime root, never
// there, so they are skipped before anything else is loaded; the hooks check the bundle id again.
#include <dlfcn.h>
#include <limits.h>
#include <mach-o/dyld.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int user_app(void) {
    char exe[PATH_MAX];
    uint32_t size = sizeof exe;
    if (_NSGetExecutablePath(exe, &size) != 0) return 0;
    return strstr(exe, "/Containers/Bundle/Application/") && strstr(exe, ".app/") && !strstr(exe, ".appex/");
}

__attribute__((constructor)) static void simee_camera_load(void) {
    const char *socket = getenv("SIMEE_CAMERA_SOCKET");
    if (!socket || !*socket || !user_app()) return;
    Dl_info self;
    if (!dladdr((const void *)simee_camera_load, &self) || !self.dli_fname) return;
    char hooks[PATH_MAX];
    const char *slash = strrchr(self.dli_fname, '/');
    int dir = slash ? (int)(slash - self.dli_fname) : 0;
    if (snprintf(hooks, sizeof hooks, "%.*s/libsimee-camera-hooks.dylib", dir, self.dli_fname) >= (int)sizeof hooks) return;
    if (!dlopen(hooks, RTLD_NOW | RTLD_LOCAL)) fprintf(stderr, "[simee-camera] can't load %s: %s\n", hooks, dlerror());
}
