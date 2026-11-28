//! What each method in [`METHODS`](crate::METHODS) does and where it belongs, beyond its name and
//! parameters: its effect (what a front end's approval prompts read: the MCP server's tool
//! annotations) and its group (the MCP server's toolsets). Every method is classified here on
//! purpose (a test fails for one that isn't), since a client may auto-approve what is read-only.

/// What a method does to a device or the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Changes nothing, on the device or the host.
    Read,
    /// Sets something to a value; repeating it changes nothing more.
    Set,
    /// Does something each time it is called (input, a boot) without destroying anything.
    Act,
    /// May lose data, run code or change what's running; repeating it can differ.
    Danger,
    /// Destroys or overwrites something; repeating it changes nothing more.
    Destroy,
}

impl Effect {
    /// Whether it changes nothing.
    pub fn read_only(self) -> bool {
        self == Effect::Read
    }

    /// Whether it may destroy or overwrite something, or run code that might.
    pub fn destructive(self) -> bool {
        matches!(self, Effect::Danger | Effect::Destroy)
    }

    /// Whether calling it again with the same arguments changes nothing more.
    pub fn idempotent(self) -> bool {
        matches!(self, Effect::Read | Effect::Set | Effect::Destroy)
    }

    /// One sentence for a description an agent reads.
    pub fn sentence(self) -> &'static str {
        match self {
            Effect::Read => "Read-only: changes nothing on the device or the host.",
            Effect::Set => "Sets a value on the device; repeating it changes nothing more.",
            Effect::Act => "Acts on the device each time it is called.",
            Effect::Danger => "Caution: runs code, restarts or replaces state, or writes files; repeating it can differ.",
            Effect::Destroy => {
                "Destructive: deletes, stops or overwrites something; deleted or overwritten data is not recovered, and a stopped device or app can be started again."
            }
        }
    }
}

/// The effect of `method`, or `None` for one nobody has classified (a front end then assumes the
/// worst: it writes, and may destroy).
pub fn effect(method: &str) -> Option<Effect> {
    use Effect::*;
    Some(match method {
        "prefs_stores" | "prefs_read" | "prefs_watch" => Read,
        "prefs_write" | "prefs_delete" | "prefs_batch" => Destroy,
        // Read a corpse of the app: nothing on the device changes.
        "mem_leaks" | "mem_heap" | "mem_vmmap" | "mem_history" => Read,
        // A stream sends the screen to a URL (and, with input, takes a viewer's touches); status
        // only renews a lease; stop ends it.
        "stream_start" => Danger,
        "stream_status" | "streams" => Read,
        "stream_stop" => Destroy,
        "help" | "identity" | "host" | "devices_usage" | "form_factors" | "devices" | "runtimes" | "device_types" | "screen"
        | "screenshot" | "wait_idle" | "apps" | "ca_debug_state" | "logs" | "processes" | "connections" | "traffic" | "containers"
        | "ls" | "read" | "du" | "crash_reports" | "info" | "defaults_read" | "network_state" | "network_flows" | "network_env"
        | "audio_state" | "settle" | "admission_check" | "camera_sources" | "camera_links" => Read,
        // Saved with the device and applied live: linking the same source again changes nothing.
        "camera_link" | "camera_unlink" => Set,
        "appearance" | "location" | "status_bar" | "clear_status_bar" | "content_size" | "increase_contrast" | "dynamic_island"
        | "backlight" | "keyboard_language" | "hardware_keyboard" | "ca_debug" | "network_clear" | "audio_volume" | "audio_route"
        | "audio_mute" | "fold" | "carplay" | "profile" | "privacy" => Set,
        "create" | "boot" | "boot_many" | "clone" | "tap" | "swipe" | "touch" | "button" | "type" | "key" | "open_url" | "install"
        | "memory_warning" | "biometric" | "rotate" | "rotary" | "watch_gesture" | "remote_swipe" | "xr" | "touchpad" | "stylus"
        | "pointer" | "vehicle" | "audio_capture_stop" | "contrast_check" | "import" => Act,
        // Runs programs, starts apps with an environment, replaces a network profile (inject "boot"
        // reboots the device), delivers a payload, writes a recording or a bundle to a path, drives
        // the console, signals a process. `notify` is here for Android, where it is an `am broadcast`
        // with arbitrary typed extras, sent as the shell user (who holds many development
        // permissions) to any exported receiver of any app: closer to `push` than to a tap.
        "exec" | "launch" | "push" | "notify" | "network_shape" | "console" | "audio_capture_start" | "kill" | "export" => Danger,
        "delete" | "erase" | "shutdown" | "rm" | "uninstall" | "terminate" | "reset_keychain" | "clear_container" | "write"
        | "defaults_write" => Destroy,
        _ => return None,
    })
}

/// Whether `method` reaches beyond the device and the host: `open_url` hands a URL to the device,
/// which may fetch it from anywhere, and `stream_start` sends the screen to a media server that
/// relays it to any viewer. Everything else stays on this machine.
pub fn open_world(method: &str) -> bool {
    matches!(method, "open_url" | "stream_start")
}

/// What a method is for: the MCP server's toolsets, which a client picks from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    /// Listing, creating, booting and destroying devices.
    Lifecycle,
    /// Installing, launching and stopping apps, and what they may do.
    Apps,
    /// Touches, buttons, keys, gestures and the hardware a form factor has.
    Input,
    /// The screen, contrast and logs.
    Observe,
    /// The device's settings and simulated conditions.
    Settings,
    /// Processes, sockets, memory, render overlays, programs, the console.
    Debug,
    /// The device's files and preference domains.
    Files,
    /// An app's typed preference stores.
    Prefs,
    /// Network shaping.
    Network,
    /// The device's audio on the host.
    Audio,
    /// The screen streamed over WebRTC (WHIP).
    Stream,
    /// The device's cameras and the host's camera sources.
    Camera,
    /// The host and the server itself.
    Host,
}

impl Group {
    pub const ALL: [Group; 13] = [
        Group::Lifecycle,
        Group::Apps,
        Group::Input,
        Group::Observe,
        Group::Settings,
        Group::Debug,
        Group::Files,
        Group::Prefs,
        Group::Network,
        Group::Audio,
        Group::Stream,
        Group::Camera,
        Group::Host,
    ];

    /// The name a client picks it by.
    pub fn name(self) -> &'static str {
        match self {
            Group::Lifecycle => "lifecycle",
            Group::Apps => "apps",
            Group::Input => "input",
            Group::Observe => "observe",
            Group::Settings => "settings",
            Group::Debug => "debug",
            Group::Files => "files",
            Group::Prefs => "prefs",
            Group::Network => "network",
            Group::Audio => "audio",
            Group::Stream => "stream",
            Group::Camera => "camera",
            Group::Host => "host",
        }
    }

    pub fn from_name(name: &str) -> Option<Group> {
        Group::ALL.into_iter().find(|g| g.name() == name)
    }
}

/// The group of `method`, or `None` for one nobody has placed.
pub fn group(method: &str) -> Option<Group> {
    use Group::*;
    Some(match method {
        "help" | "identity" | "host" | "devices_usage" => Host,
        "form_factors" | "devices" | "runtimes" | "device_types" | "create" | "delete" | "boot" | "boot_many" | "admission_check"
        | "settle" | "profile" | "clone" | "export" | "import" | "shutdown" | "erase" | "info" => Lifecycle,
        "screen" | "screenshot" | "wait_idle" | "contrast_check" | "logs" => Observe,
        "tap" | "swipe" | "touch" | "button" | "type" | "key" | "rotate" | "fold" | "xr" | "touchpad" | "stylus" | "rotary"
        | "watch_gesture" | "remote_swipe" | "pointer" | "biometric" => Input,
        "open_url" | "install" | "uninstall" | "launch" | "terminate" | "apps" | "privacy" | "push" => Apps,
        "appearance" | "location" | "status_bar" | "clear_status_bar" | "content_size" | "increase_contrast" | "dynamic_island"
        | "backlight" | "keyboard_language" | "hardware_keyboard" | "memory_warning" | "notify" | "carplay" | "vehicle" => Settings,
        "ca_debug" | "ca_debug_state" | "exec" | "processes" | "kill" | "connections" | "traffic" | "mem_leaks" | "mem_heap"
        | "mem_vmmap" | "mem_history" | "crash_reports" | "console" => Debug,
        "containers" | "ls" | "read" | "write" | "rm" | "du" | "clear_container" | "defaults_read" | "defaults_write"
        | "reset_keychain" => Files,
        "prefs_stores" | "prefs_read" | "prefs_write" | "prefs_delete" | "prefs_batch" | "prefs_watch" => Prefs,
        "network_shape" | "network_state" | "network_flows" | "network_clear" | "network_env" => Network,
        "audio_state" | "audio_volume" | "audio_route" | "audio_capture_start" | "audio_capture_stop" | "audio_mute" => Audio,
        "stream_start" | "stream_status" | "stream_stop" | "streams" => Stream,
        "camera_sources" | "camera_link" | "camera_unlink" | "camera_links" => Camera,
        _ => return None,
    })
}

/// The methods a general agent needs to drive a device: find and boot it, see its screen, touch,
/// type and press buttons, install and run an app, read its logs. The MCP server's default
/// toolset (`core`), with its device view.
pub const CORE: &[&str] = &[
    "devices",
    "boot",
    "shutdown",
    "info",
    "screen",
    "screenshot",
    "tap",
    "swipe",
    "type",
    "key",
    "button",
    "rotate",
    "open_url",
    "install",
    "uninstall",
    "launch",
    "terminate",
    "apps",
    "logs",
    "appearance",
    "location",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::METHODS;

    #[test]
    fn every_method_has_an_effect_and_a_group() {
        let unclassified: Vec<&str> = METHODS.iter().map(|(m, ..)| *m).filter(|m| effect(m).is_none()).collect();
        assert!(unclassified.is_empty(), "classify these in `effect` in catalog.rs: {unclassified:?}");
        let unplaced: Vec<&str> = METHODS.iter().map(|(m, ..)| *m).filter(|m| group(m).is_none()).collect();
        assert!(unplaced.is_empty(), "place these in `group` in catalog.rs: {unplaced:?}");
        for m in CORE {
            assert!(METHODS.iter().any(|(name, ..)| name == m), "{m} is not a method");
        }
        for g in Group::ALL {
            assert_eq!(Group::from_name(g.name()), Some(g));
            assert!(METHODS.iter().any(|(m, ..)| group(m) == Some(g)), "{g:?} is empty");
        }
        assert_eq!(effect("a_new_method"), None);
    }

    #[test]
    fn what_destroys_runs_code_or_overwrites_is_destructive() {
        for m in [
            "delete",
            "erase",
            "kill",
            "clear_container",
            "reset_keychain",
            "shutdown",
            "uninstall",
            "terminate",
            "rm",
            "write",
            "defaults_write",
            "prefs_write",
            "prefs_delete",
            "prefs_batch",
            "exec",
            "launch",
            "push",
            "network_shape",
            "console",
            "audio_capture_start",
            "export",
            "stream_start",
            "stream_stop",
        ] {
            let e = effect(m).expect(m);
            assert!(e.destructive() && !e.read_only(), "{m}");
        }
        for m in ["devices", "screenshot", "logs", "processes", "prefs_read", "mem_leaks"] {
            assert!(effect(m).is_some_and(Effect::read_only), "{m}");
        }
        assert!(!effect("tap").expect("tap").idempotent() && effect("appearance").expect("appearance").idempotent());
        assert!(open_world("open_url") && open_world("stream_start") && !open_world("tap") && !open_world("stream_status"));
    }
}
