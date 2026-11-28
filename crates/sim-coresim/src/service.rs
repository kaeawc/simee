//! Service context, device sets and devices.

use std::process::Command;
use std::ptr;

use block2::{Block, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{msg_send, sel};
use objc2_foundation::{NSError, NSString};
use sim_core::device::{DeviceInfo, DeviceState, DeviceTypeInfo, RuntimeInfo, Selection, select};
use sim_core::form_factor::{apple_kind, apple_type_form_factor};
use sim_core::hid::Orientation;
use sim_core::platform::Platform;

use crate::display::Display;
use crate::error::{Error, Result};
use crate::hid::Hid;
use crate::rt::{self, CGSize, Library, Obj};

const CORE_SIMULATOR: &str = "/Library/Developer/PrivateFrameworks/CoreSimulator.framework/CoreSimulator";

/// Where `xcode-select -s` records the selected developer directory.
const XCODE_SELECT_LINK: &str = "/var/db/xcode_select_link";

/// The active developer directory: `DEVELOPER_DIR`, else the directory `xcode-select -s` linked,
/// else `xcode-select -p` (read once per process: every context, API connection, capture and boot
/// thread asks). Reading the link spares each CLI run a process spawn.
pub fn developer_dir() -> Result<String> {
    if let Ok(dir) = std::env::var("DEVELOPER_DIR")
        && !dir.is_empty()
    {
        return Ok(dir);
    }
    static SELECTED: std::sync::OnceLock<std::result::Result<String, String>> = std::sync::OnceLock::new();
    SELECTED
        .get_or_init(|| {
            if let Some(dir) = linked_dir(std::fs::read_link(XCODE_SELECT_LINK).ok(), |p| p.is_dir()) {
                return Ok(dir);
            }
            let out = Command::new("/usr/bin/xcode-select").arg("-p").output().map_err(|e| format!("xcode-select -p: {e}"))?;
            let dir = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            if out.status.success() && !dir.is_empty() { Ok(dir) } else { Err("xcode-select -p returned nothing".to_owned()) }
        })
        .clone()
        .map_err(Error::Unavailable)
}

/// The developer directory the selection link names, when it is usable as is: an absolute path to
/// an existing directory, as `xcode-select -p` would print it. Anything else (no link, a relative
/// or dangling target) leaves the answer to `xcode-select -p` itself.
fn linked_dir(target: Option<std::path::PathBuf>, is_dir: impl Fn(&std::path::Path) -> bool) -> Option<String> {
    let target = target?;
    let dir = target.to_str()?.trim_end_matches('/');
    (target.is_absolute() && !dir.is_empty() && is_dir(&target)).then(|| dir.to_owned())
}

/// SimulatorKit inside a frameworks directory.
const SIMULATOR_KIT: &str = "SimulatorKit.framework/SimulatorKit";

/// Where SimulatorKit may be for a developer directory, in the order to try: Xcode 27 ships it in
/// the app's `Contents/SharedFrameworks`, earlier Xcodes in `Contents/Developer/Library/
/// PrivateFrameworks`. A developer directory that isn't an app's `Contents/Developer` (the
/// Command Line Tools, which have none) gets only the second.
fn simulator_kit_candidates(developer_dir: &str) -> Vec<String> {
    let dir = developer_dir.trim_end_matches('/');
    let shared = dir.strip_suffix("/Contents/Developer").map(|app| format!("{app}/Contents/SharedFrameworks/{SIMULATOR_KIT}"));
    shared.into_iter().chain([format!("{dir}/Library/PrivateFrameworks/{SIMULATOR_KIT}")]).collect()
}

/// Loads the first SimulatorKit of [`simulator_kit_candidates`] that opens, else
/// [`Error::Unavailable`] naming every path tried.
fn open_simulator_kit(developer_dir: &str) -> Result<Library> {
    let candidates = simulator_kit_candidates(developer_dir);
    candidates
        .iter()
        .find_map(|path| Library::open(path).ok())
        .ok_or_else(|| Error::Unavailable(format!("cannot load SimulatorKit (tried {})", candidates.join(", "))))
}

/// What a boot hands the service besides the device ([`Device::boot_with`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct BootOptions<'a> {
    /// Extra environment for the device's launchd (the `env` option).
    pub env: &'a [(String, String)],
    /// launchd labels to disable (`true`) or enable (`false`) for this boot (`disabled_jobs`, what
    /// `simctl boot --disabledJob=`/`--enabledJob=` pass). CoreSimulator writes them, as they are,
    /// to [`sim_core::launchd::boot_plist_path`], replacing that file (and deletes it on a boot
    /// without them); only a runtime whose launchd_sim reads that directory (iOS 18.0) applies them.
    pub jobs: &'a [(String, bool)],
}

/// The options dictionary for `bootWithOptions:`, its `disabled_jobs` under `jobs_key`.
fn boot_options(opts: &BootOptions<'_>, jobs_key: &str) -> Obj {
    let options = rt::mutable_dictionary();
    if !opts.env.is_empty() {
        rt::dict_set(&options, "env", &rt::string_dictionary(opts.env));
    }
    if !opts.jobs.is_empty() {
        rt::dict_set(&options, jobs_key, &rt::bool_dictionary(opts.jobs));
    }
    options
}

/// CoreSimulator's key for the disabled jobs (its exported `SimDeviceBootKeyDisabledJobs`).
fn disabled_jobs_key() -> String {
    jobs_key_or_default(
        rt::string_symbol(c"SimDeviceBootKeyDisabledJobs").ok().and_then(|k| k.downcast::<NSString>().ok()).map(|s| s.to_string()),
    )
}

/// The exported key, else the literal CoreSimulator 1174 uses (a service that doesn't know the key
/// ignores it).
fn jobs_key_or_default(exported: Option<String>) -> String {
    exported.filter(|k| !k.is_empty()).unwrap_or_else(|| "disabled_jobs".to_owned())
}

#[cfg(test)]
mod boot_options_tests {
    use objc2::class;

    use super::*;

    /// `obj` as an XML property list, as CoreSimulator writes the jobs to disk.
    fn xml_plist(obj: &AnyObject) -> String {
        let mut error: *mut NSError = ptr::null_mut();
        // SAFETY: takes a property-list object, a format (100: XML), options (0) and an NSError**;
        // returns nil on failure.
        let data: Option<Obj> = unsafe {
            msg_send![class!(NSPropertyListSerialization), dataWithPropertyList: obj, format: 100usize, options: 0usize, error: &mut error]
        };
        let data = data.expect("serialised");
        // SAFETY: `length` returns NSUInteger and `bytes` a pointer to that many bytes, alive while `data` is.
        let bytes = unsafe {
            let len: usize = msg_send![&*data, length];
            let ptr: *const std::ffi::c_void = msg_send![&*data, bytes];
            std::slice::from_raw_parts(ptr.cast::<u8>(), len).to_vec()
        };
        String::from_utf8(bytes).expect("utf-8")
    }

    #[test]
    fn the_jobs_go_in_as_a_dictionary_of_booleans_beside_the_environment() {
        let env = [("SIMEE".to_owned(), "1".to_owned())];
        let jobs = [("com.apple.PosterBoard".to_owned(), true), ("com.apple.example".to_owned(), false)];
        let xml = xml_plist(&boot_options(&BootOptions { env: &env, jobs: &jobs }, "disabled_jobs"));
        let compact: String = xml.split_whitespace().collect();
        assert!(
            compact.contains(
                "<key>disabled_jobs</key><dict><key>com.apple.PosterBoard</key><true/><key>com.apple.example</key><false/></dict>"
            ),
            "{xml}"
        );
        assert!(compact.contains("<key>env</key><dict><key>SIMEE</key><string>1</string></dict>"), "{xml}");
    }

    #[test]
    fn without_jobs_or_environment_the_options_are_empty() {
        let compact: String = xml_plist(&boot_options(&BootOptions::default(), "disabled_jobs")).split_whitespace().collect();
        assert!(compact.contains("<dict/>"), "{compact}");
        let env = [("A".to_owned(), "b".to_owned())];
        let compact: String = xml_plist(&boot_options(&BootOptions { env: &env, jobs: &[] }, "k")).split_whitespace().collect();
        assert!(!compact.contains("<key>k</key>"), "no jobs key without jobs: {compact}");
    }

    #[test]
    fn the_jobs_key_is_coresimulators_export_or_its_literal() {
        assert_eq!(jobs_key_or_default(Some("disabled_jobs_v2".into())), "disabled_jobs_v2");
        assert_eq!(jobs_key_or_default(None), "disabled_jobs", "the symbol is missing");
        assert_eq!(jobs_key_or_default(Some(String::new())), "disabled_jobs");
        // With CoreSimulator loaded the export is the literal (1174.21).
        if Library::open(CORE_SIMULATOR).is_ok() {
            assert_eq!(disabled_jobs_key(), "disabled_jobs");
        }
    }
}

#[cfg(test)]
mod developer_dir_tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn the_selection_link_is_used_only_when_it_names_a_directory() {
        let xcode = PathBuf::from("/Applications/Xcode.app/Contents/Developer");
        assert_eq!(linked_dir(Some(xcode.clone()), |_| true).as_deref(), Some("/Applications/Xcode.app/Contents/Developer"));
        assert_eq!(
            linked_dir(Some(PathBuf::from("/Library/Developer/CommandLineTools/")), |_| true).as_deref(),
            Some("/Library/Developer/CommandLineTools")
        );
        assert_eq!(linked_dir(Some(xcode), |_| false), None, "a removed Xcode: ask xcode-select");
        assert_eq!(linked_dir(Some(PathBuf::from("Xcode.app/Contents/Developer")), |_| true), None, "relative");
        assert_eq!(linked_dir(Some(PathBuf::from("/")), |_| true), None);
        assert_eq!(linked_dir(None, |_| true), None, "no link");
    }

    #[test]
    fn simulator_kit_is_looked_for_where_xcode_27_moved_it_then_where_it_was() {
        let kit = "SimulatorKit.framework/SimulatorKit";
        assert_eq!(
            simulator_kit_candidates("/Applications/Xcode-27.2.0.app/Contents/Developer"),
            [
                format!("/Applications/Xcode-27.2.0.app/Contents/SharedFrameworks/{kit}"),
                format!("/Applications/Xcode-27.2.0.app/Contents/Developer/Library/PrivateFrameworks/{kit}"),
            ]
        );
        assert_eq!(
            simulator_kit_candidates("/Applications/Xcode.app/Contents/Developer/"),
            [
                format!("/Applications/Xcode.app/Contents/SharedFrameworks/{kit}"),
                format!("/Applications/Xcode.app/Contents/Developer/Library/PrivateFrameworks/{kit}"),
            ],
            "a trailing slash"
        );
        assert_eq!(
            simulator_kit_candidates("/Library/Developer/CommandLineTools"),
            [format!("/Library/Developer/CommandLineTools/Library/PrivateFrameworks/{kit}")],
            "not inside an app's Contents: only the developer directory's own"
        );
        assert_eq!(simulator_kit_candidates("/"), [format!("/Library/PrivateFrameworks/{kit}")]);
    }

    #[test]
    fn a_missing_simulator_kit_is_unavailable_and_names_every_path_tried() {
        let Err(Error::Unavailable(msg)) = open_simulator_kit("/nonexistent/Xcode.app/Contents/Developer") else {
            panic!("expected Error::Unavailable");
        };
        assert!(msg.contains("/nonexistent/Xcode.app/Contents/SharedFrameworks/"), "{msg}");
        assert!(msg.contains("/nonexistent/Xcode.app/Contents/Developer/Library/PrivateFrameworks/"), "{msg}");
    }

    /// The link against the spawn this replaces (no simulator service is touched).
    ///     cargo test --release -p simee-coresim developer_dir_cost -- --ignored --nocapture
    #[test]
    #[ignore = "benchmark"]
    fn developer_dir_cost() {
        let median = |f: &dyn Fn() -> String| {
            let mut runs: Vec<(f64, String)> = (0..15)
                .map(|_| {
                    let t = std::time::Instant::now();
                    let dir = f();
                    (t.elapsed().as_secs_f64() * 1000.0, dir)
                })
                .collect();
            runs.sort_by(|a, b| a.0.total_cmp(&b.0));
            runs.swap_remove(runs.len() / 2)
        };
        let (spawn, printed) = median(&|| {
            let out = Command::new("/usr/bin/xcode-select").arg("-p").output().expect("xcode-select");
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        });
        let (link, linked) = median(&|| linked_dir(std::fs::read_link(XCODE_SELECT_LINK).ok(), |p| p.is_dir()).unwrap_or_default());
        assert_eq!(linked, printed, "the link names what xcode-select prints");
        println!("developer dir: xcode-select -p {spawn:.3} ms, link {link:.3} ms ({linked})");
    }
}

/// Status bar overrides. Raw values follow the service's enums: data network 0 = hidden, 1 = Wi-Fi,
/// 2 = 3G, 3 = 4G, 4 = LTE, 5 = LTE-A, 6 = LTE+, 7 = 5G, 8 = 5G+, 9 = 5G UWB, 10 = 5G UC; Wi-Fi and
/// cellular modes 0 = not supported, 1 = searching, 2 = failed, 3 = active; battery state
/// 0 = charging, 1 = charged, 2 = discharging.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusBar {
    pub time: Option<String>,
    pub data_network: Option<u64>,
    pub wifi: Option<(u64, u64)>,
    pub cellular: Option<(u64, u64)>,
    pub operator: Option<String>,
    pub battery: Option<(u64, u64)>,
}

/// Output of a process run inside the device.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecOutput {
    pub pid: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Light or dark UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Appearance {
    Light,
    Dark,
}

/// A connection to the simulator service and one device set: the default set, or another
/// directory of devices ([`Context::open`]).
pub struct Context {
    context: Obj,
    set: Obj,
    kit: Library,
}

impl Context {
    /// Connects to the service for the active developer directory, on the default device set.
    pub fn new() -> Result<Self> {
        Self::open(None)
    }

    /// Connects to the service on the device set at `set` (an absolute directory, made if
    /// missing), or on the default set (`~/Library/Developer/CoreSimulator/Devices`) for `None`
    /// (#409). Simulator.app opens a window for each device of the default set that boots; a
    /// device of another set (as `simctl --set` makes) boots with no window.
    pub fn open(set: Option<&str>) -> Result<Self> {
        let dir = developer_dir()?;
        Library::open(CORE_SIMULATOR)?;
        let kit = open_simulator_kit(&dir)?;
        let cls = rt::class(c"SimServiceContext")?;
        if cls.class_method(sel!(sharedServiceContextForDeveloperDir:error:)).is_none() {
            return Err(Error::Unavailable("sharedServiceContextForDeveloperDir:error:".into()));
        }
        // SAFETY: the class method exists (checked) and takes (NSString, NSError**), returning an object.
        let context: std::result::Result<Obj, Retained<NSError>> =
            unsafe { msg_send![cls, sharedServiceContextForDeveloperDir: &*rt::nsstring(&dir), error: _] };
        let context = context.map_err(|e| rt::service_error("connect", &e))?;
        let set: std::result::Result<Obj, Retained<NSError>> = match set {
            None => {
                rt::require(&context, sel!(defaultDeviceSetWithError:))?;
                // SAFETY: the method exists (checked) and takes NSError**, returning the device set.
                unsafe { msg_send![&*context, defaultDeviceSetWithError: _] }
            }
            Some(path) => {
                if !path.starts_with('/') {
                    return Err(Error::Invalid(format!("a device set is an absolute path, not `{path}`")));
                }
                std::fs::create_dir_all(path).map_err(|e| Error::Invalid(format!("device set {path}: {e}")))?;
                rt::require(&context, sel!(deviceSetWithPath:error:))?;
                // SAFETY: the method exists (checked) and takes (NSString, NSError**), returning the
                // device set at that path (nil with the error set on failure).
                unsafe { msg_send![&*context, deviceSetWithPath: &*rt::nsstring(path), error: _] }
            }
        };
        let set = set.map_err(|e| rt::service_error("open device set", &e))?;
        Ok(Self { context, set, kit })
    }

    /// The directory of this connection's device set.
    pub fn set_path(&self) -> String {
        rt::string_prop(&self.set, sel!(setPath))
    }

    /// All devices in the set, sorted by runtime then name.
    pub fn devices(&self) -> Result<Vec<Device>> {
        let mut devices = self.unsorted_devices()?;
        devices.sort_by_cached_key(|d| {
            let i = d.info();
            (i.runtime_id, i.name, i.udid)
        });
        Ok(devices)
    }

    /// Resolves a UDID, a name, or `booted`.
    pub fn device(&self, query: &str) -> Result<Device> {
        match self.by_udid(query) {
            Some(device) => Ok(device),
            None => self.resolve(query).map(|(device, _)| device),
        }
    }

    /// The device whose UDID `query` is (in any case), looked up in the set's UDID-keyed
    /// dictionary: one hash lookup however many simulators the host has. The dictionary is the
    /// service's live one (`-devicesByUDID` copies it per call; the copy is lazy), so a deleted
    /// device misses and a renamed one reads its new name. `None` when `query` isn't a UDID, no
    /// device has it, or the service has no such dictionary: the caller then selects.
    fn by_udid(&self, query: &str) -> Option<Device> {
        if !sim_core::device::is_udid(query) {
            return None;
        }
        let devices = rt::optional_prop(&self.set, sel!(devicesByUDID))?;
        let cls = rt::class(c"NSUUID").ok()?;
        // SAFETY: `alloc` returns an uninitialized instance, consumed by the init call below.
        let alloc: objc2::rc::Allocated<AnyObject> = unsafe { msg_send![cls, alloc] };
        // SAFETY: `-[NSUUID initWithUUIDString:]` takes an NSString and returns nil unless it is a
        // UUID (any case).
        let key: Option<Obj> = unsafe { msg_send![alloc, initWithUUIDString: &*rt::nsstring(query)] };
        // SAFETY: the dictionary maps NSUUIDs to SimDevices; `objectForKey:` returns nil or the value.
        let obj: Option<Obj> = unsafe { msg_send![&*devices, objectForKey: &*key?] };
        obj.map(|obj| Device { obj, kit: self.kit })
    }

    /// Resolves a UDID, a name, or `booted`, with the device's info (read while selecting).
    pub fn resolve(&self, query: &str) -> Result<(Device, DeviceInfo)> {
        if let Some(device) = self.by_udid(query) {
            let info = device.info();
            return Ok((device, info));
        }
        let mut devices = self.unsorted_devices()?;
        // `select` reads only the UDID, name and state, and doesn't depend on order: read just
        // those of every device (the UDID alone when it names one), then the full info of the
        // match.
        let mut udids: Vec<String> = Vec::with_capacity(devices.len());
        let by_udid = devices.iter().position(|d| {
            udids.push(d.udid());
            udids.last().is_some_and(|u| u.eq_ignore_ascii_case(query))
        });
        let i = match by_udid {
            Some(i) => i,
            None => {
                let keys: Vec<DeviceInfo> = devices.iter().zip(udids).map(|(d, udid)| d.selection_info_with(udid)).collect();
                match select(&keys, query) {
                    Selection::One(i) => i,
                    Selection::None => return Err(Error::NotFound(query.to_owned())),
                    Selection::Ambiguous => return Err(Error::Ambiguous(query.to_owned())),
                }
            }
        };
        let device = devices.swap_remove(i);
        let info = device.info();
        Ok((device, info))
    }

    /// The devices to select from: [`select`] keys (UDID, name and state) only, unsorted; the
    /// API resolves a query across platforms with these.
    pub fn selection_infos(&self) -> Result<Vec<DeviceInfo>> {
        Ok(self.unsorted_devices()?.iter().map(Device::selection_info).collect())
    }

    /// Every device in the set, in no order: the UDID-keyed dictionary's values where the service
    /// has one, as `-[SimDeviceSet devices]` sorts them all (`-[SimDevice compare:]`, which reads
    /// each runtime) on every call, ~0.9 ms with 89 simulators against microseconds.
    fn unsorted_devices(&self) -> Result<Vec<Device>> {
        let list: Obj = match rt::optional_prop(&self.set, sel!(devicesByUDID)) {
            // SAFETY: `allValues` on an NSDictionary returns a non-nil NSArray.
            Some(by_udid) => unsafe { msg_send![&*by_udid, allValues] },
            None => rt::object_prop(&self.set, sel!(devices))?,
        };
        Ok(rt::array_items(&list).into_iter().map(|obj| Device { obj, kit: self.kit }).collect())
    }

    /// Installed runtimes.
    pub fn runtimes(&self) -> Result<Vec<RuntimeInfo>> {
        Ok(self.runtime_objects()?.iter().map(|r| runtime_info(r)).collect())
    }

    /// Known device types.
    pub fn device_types(&self) -> Result<Vec<DeviceTypeInfo>> {
        Ok(self.device_type_objects()?.iter().map(|t| device_type_info(t)).collect())
    }

    fn runtime_objects(&self) -> Result<Vec<Obj>> {
        Ok(rt::array_items(&*rt::object_prop(&self.context, sel!(supportedRuntimes))?))
    }

    fn device_type_objects(&self) -> Result<Vec<Obj>> {
        Ok(rt::array_items(&*rt::object_prop(&self.context, sel!(supportedDeviceTypes))?))
    }

    /// Creates a device. `device_type` and `runtime` match by identifier or name.
    pub fn create(&self, name: &str, device_type: &str, runtime: &str) -> Result<Device> {
        let matches = |obj: &AnyObject, q: &str| {
            rt::string_prop(obj, sel!(identifier)).eq_ignore_ascii_case(q) || rt::string_prop(obj, sel!(name)).eq_ignore_ascii_case(q)
        };
        let ty =
            self.device_type_objects()?.into_iter().find(|t| matches(t, device_type)).ok_or_else(|| Error::NotFound(device_type.into()))?;
        let rt_obj = self.runtime_objects()?.into_iter().find(|r| matches(r, runtime)).ok_or_else(|| Error::NotFound(runtime.into()))?;
        rt::require(&self.set, sel!(createDeviceWithType:runtime:name:error:))?;
        // SAFETY: the method exists (checked) and takes (SimDeviceType, SimRuntime, NSString, NSError**).
        let created: std::result::Result<Obj, Retained<NSError>> =
            unsafe { msg_send![&*self.set, createDeviceWithType: &*ty, runtime: &*rt_obj, name: &*rt::nsstring(name), error: _] };
        Ok(Device { obj: created.map_err(|e| rt::service_error("create", &e))?, kit: self.kit })
    }

    /// Creates a device named `name` as a copy of `device` (shut down): its type, runtime and the
    /// whole data directory, settings and installed apps included. The service copies the data
    /// with APFS clones (`simctl clone`), so a copy costs little disk until either device writes.
    pub fn clone_device(&self, device: &Device, name: &str) -> Result<Device> {
        rt::require(&self.set, sel!(cloneDevice:name:error:))?;
        // SAFETY: the method exists (checked) and takes (SimDevice, NSString, NSError**), returning
        // the new SimDevice (nil with the error set on failure).
        let cloned: std::result::Result<Obj, Retained<NSError>> =
            unsafe { msg_send![&*self.set, cloneDevice: &*device.obj, name: &*rt::nsstring(name), error: _] };
        Ok(Device { obj: cloned.map_err(|e| rt::service_error("clone", &e))?, kit: self.kit })
    }

    /// Deletes a device and its data.
    pub fn delete(&self, device: &Device) -> Result<()> {
        rt::require(&self.set, sel!(deleteDevice:error:))?;
        // SAFETY: the method exists (checked) and takes (SimDevice, NSError**), returning BOOL.
        let done: std::result::Result<(), Retained<NSError>> = unsafe { msg_send![&*self.set, deleteDevice: &*device.obj, error: _] };
        done.map_err(|e| rt::service_error("delete", &e))
    }
}

fn runtime_info(r: &AnyObject) -> RuntimeInfo {
    RuntimeInfo {
        identifier: rt::string_prop(r, sel!(identifier)),
        name: rt::string_prop(r, sel!(name)),
        version: rt::string_prop(r, sel!(versionString)),
        build: rt::string_prop(r, sel!(buildVersionString)),
        // SAFETY: `available` is a BOOL getter on SimRuntime (guarded by the responds check).
        available: rt::responds(r, sel!(available)) && unsafe { msg_send![r, available] },
        platform: Platform::Ios,
    }
}

fn device_type_info(t: &AnyObject) -> DeviceTypeInfo {
    let size: CGSize = if rt::responds(t, sel!(mainScreenSize)) {
        // SAFETY: `mainScreenSize` returns CGSize by value (checked to exist).
        unsafe { msg_send![t, mainScreenSize] }
    } else {
        CGSize::default()
    };
    let scale: f32 = if rt::responds(t, sel!(mainScreenScale)) {
        // SAFETY: `mainScreenScale` returns float (checked to exist).
        unsafe { msg_send![t, mainScreenScale] }
    } else {
        0.0
    };
    let product_family = rt::string_prop(t, sel!(productFamily));
    let identifier = rt::string_prop(t, sel!(identifier));
    DeviceTypeInfo {
        identifier: identifier.clone(),
        name: rt::string_prop(t, sel!(name)),
        model: rt::string_prop(t, sel!(modelIdentifier)),
        product_family: product_family.clone(),
        screen_width: size.width,
        screen_height: size.height,
        screen_scale: f64::from(scale),
        platform: Platform::Ios,
        form_factor: apple_type_form_factor(&identifier, &product_family),
    }
}

/// One simulated device.
pub struct Device {
    pub(crate) obj: Obj,
    kit: Library,
}

/// A registered state handler ([`Device::watch_state`]); dropping it unregisters the handler.
pub struct StateWatch {
    device: Obj,
    handle: u64,
    _block: RcBlock<dyn Fn(*mut AnyObject)>,
}

impl Drop for StateWatch {
    fn drop(&mut self) {
        // SAFETY: the device responds to the selector (checked at registration), which takes
        // (unsigned long long, NSError **) and returns BOOL; the handle is the registration's.
        let done: std::result::Result<(), Retained<NSError>> =
            unsafe { msg_send![&*self.device, unregisterNotificationHandler: self.handle, error: _] };
        done.ok();
    }
}

/// A failed launch's error. One that SpringBoard refused because it still holds the app as being
/// updated (`LaunchServicesDataMismatch`, #123) says so and what clears it; any other is the
/// service's own.
fn launch_error(bundle_id: &str, error: *mut NSError) -> Error {
    // SAFETY: the service wrote either nil or a valid autoreleased NSError into the out-parameter.
    if let Some(e) = unsafe { error.as_ref() } {
        let mut chain = Vec::new();
        error_chain(e, &mut chain);
        if being_updated(&chain) {
            return Error::Service { op: "launch", message: being_updated_message(bundle_id) };
        }
    }
    rt::raw_error("launch", error)
}

/// Each error's `BSErrorCodeDescription` (FrontBoard's name for its code) and
/// `NSLocalizedFailureReason`, outermost first, following `NSUnderlyingError` at most 8 deep.
/// SpringBoard's reason is the innermost error's: CoreSimulator wraps it twice.
fn error_chain(error: &AnyObject, out: &mut Vec<(String, String)>) {
    let Some(info) = rt::optional_prop(error, sel!(userInfo)) else { return };
    if !rt::responds(&info, sel!(objectForKey:)) {
        return;
    }
    let field = |key| rt::dict_string(&info, key).unwrap_or_default();
    out.push((field("BSErrorCodeDescription"), field("NSLocalizedFailureReason")));
    // SAFETY: `objectForKey:` takes a key object and returns nil or the value (checked to exist).
    let underlying: Option<Obj> = unsafe { msg_send![&*info, objectForKey: &*rt::nsstring("NSUnderlyingError")] };
    if let Some(underlying) = underlying.filter(|u| rt::responds(u, sel!(userInfo)))
        && out.len() < 8
    {
        error_chain(&underlying, out);
    }
}

/// Whether a launch error's chain (each error's code name and failure reason, see
/// [`error_chain`]) is SpringBoard holding the app as being updated: its code
/// `LaunchServicesDataMismatch`, or a reason that says the app "is being updated".
fn being_updated(chain: &[(String, String)]) -> bool {
    chain.iter().any(|(code, reason)| code == "LaunchServicesDataMismatch" || reason.to_lowercase().contains("is being updated"))
}

/// What a launch SpringBoard refused as being updated says: what happened and what clears it. It
/// names `LaunchServicesDataMismatch`, which callers match to remember the state.
fn being_updated_message(bundle_id: &str) -> String {
    format!(
        "SpringBoard holds {bundle_id} as being updated (LaunchServicesDataMismatch), which back-to-back installs can \
         leave behind (#123). Reboot the device (`simee shutdown <udid>`, then `simee boot <udid>`): only a reboot is \
         known to clear it"
    )
}

#[cfg(test)]
mod launch_error_tests {
    use objc2::class;

    use super::*;

    /// An NSError in `domain` whose userInfo holds `fields` and, when given, `underlying`.
    fn error(domain: &str, code: isize, fields: &[(&str, &str)], underlying: Option<&NSError>) -> Retained<NSError> {
        let info = rt::mutable_dictionary();
        for (k, v) in fields {
            rt::dict_set(&info, k, &rt::nsstring(v));
        }
        if let Some(u) = underlying {
            rt::dict_set(&info, "NSUnderlyingError", u);
        }
        // SAFETY: `+errorWithDomain:code:userInfo:` takes (NSString, NSInteger, NSDictionary) and
        // returns an autoreleased NSError.
        unsafe { msg_send![class!(NSError), errorWithDomain: &*rt::nsstring(domain), code: code, userInfo: &*info] }
    }

    /// What CoreSimulator returns when SpringBoard refuses a launch, as the service built it for a
    /// missing app on iOS 27.2: two FBSOpenApplicationService errors around FrontBoard's own.
    fn wrapped(code_name: &str, reason: &str) -> Retained<NSError> {
        let inner =
            error("FBSOpenApplicationErrorDomain", 4, &[("BSErrorCodeDescription", code_name), ("NSLocalizedFailureReason", reason)], None);
        let request = error(
            "FBSOpenApplicationServiceErrorDomain",
            4,
            &[("BSErrorCodeDescription", "InvalidRequest"), ("NSLocalizedDescription", "The request to open \"dev.simee.tiny\" failed.")],
            Some(&inner),
        );
        error(
            "FBSOpenApplicationServiceErrorDomain",
            4,
            &[
                ("BSErrorCodeDescription", "InvalidRequest"),
                ("NSLocalizedDescription", "Simulator device failed to launch dev.simee.tiny."),
            ],
            Some(&request),
        )
    }

    fn message(e: &Retained<NSError>) -> String {
        launch_error("dev.simee.tiny", Retained::as_ptr(e).cast_mut()).to_string()
    }

    #[test]
    fn a_launch_springboard_holds_as_being_updated_says_to_reboot() {
        let e = wrapped("LaunchServicesDataMismatch", "dev.simee.tiny is being updated");
        let m = message(&e);
        assert!(m.starts_with("launch failed: SpringBoard holds dev.simee.tiny as being updated (LaunchServicesDataMismatch)"), "{m}");
        assert!(m.contains("simee shutdown <udid>") && m.contains("simee boot <udid>"), "{m}");
    }

    #[test]
    fn the_reason_alone_or_the_code_alone_is_enough() {
        assert!(message(&wrapped("Unspecified", "The application \"Tiny\" is being updated.")).contains("LaunchServicesDataMismatch"));
        assert!(message(&wrapped("LaunchServicesDataMismatch", "")).contains("Reboot the device"));
    }

    #[test]
    fn any_other_refusal_keeps_the_services_own_message() {
        let e = wrapped("NotFound", "Application info provider (FBSApplicationLibrary) returned nil for \"dev.simee.tiny\"");
        assert_eq!(
            message(&e),
            "launch failed: Simulator device failed to launch dev.simee.tiny. (The request to open \"dev.simee.tiny\" failed.)"
        );
        assert_eq!(
            launch_error("dev.simee.tiny", ptr::null_mut()).to_string(),
            "launch failed: the service reported failure without an error"
        );
    }

    #[test]
    fn the_chain_is_read_outermost_first_and_stops_at_8_deep() {
        let mut chain = Vec::new();
        error_chain(&wrapped("NotFound", "gone"), &mut chain);
        let codes: Vec<&str> = chain.iter().map(|(c, _)| c.as_str()).collect();
        assert_eq!(codes, ["InvalidRequest", "InvalidRequest", "NotFound"]);
        assert_eq!(chain[2].1, "gone");
        let mut deep = error("D", 0, &[("BSErrorCodeDescription", "0")], None);
        for i in 1..20 {
            deep = error("D", i, &[("BSErrorCodeDescription", &i.to_string())], Some(&deep));
        }
        let mut chain = Vec::new();
        error_chain(&deep, &mut chain);
        assert_eq!(chain.len(), 8);
        assert!(!being_updated(&chain));
    }
}

const MACH_SEND_MSG: i32 = 0x1;
const MACH_SEND_TIMEOUT: i32 = 0x10;

unsafe extern "C" {
    static mach_task_self_: u32;
    fn mach_msg(msg: *mut std::ffi::c_void, option: i32, send_size: u32, rcv_size: u32, rcv_name: u32, timeout: u32, notify: u32) -> i32;
    fn mach_port_deallocate(task: u32, name: u32) -> i32;
}

/// Runs a `BOOL`-returning, `NSError**`-taking device method by name.
macro_rules! device_call {
    ($self:ident, $op:literal, $($sel:ident : $arg:expr),+) => {{
        rt::require(&$self.obj, sel!($($sel:)+ error:))?;
        // SAFETY: the method exists (checked); argument types match the selector's signature, which
        // objc2 additionally verifies against the runtime encoding in debug builds.
        let done: std::result::Result<(), Retained<NSError>> = unsafe { msg_send![&*$self.obj, $($sel: $arg,)+ error: _] };
        done.map_err(|e| rt::service_error($op, &e))
    }};
}

impl Device {
    /// The device's UDID.
    pub fn udid(&self) -> String {
        rt::object_prop(&self.obj, sel!(UDID)).map(|u| rt::string_prop(&u, sel!(UUIDString))).unwrap_or_default()
    }

    /// What [`select`] reads, the UDID, name and state, read as [`Device::info`] reads them; the
    /// rest left empty (an iOS device of no known kind).
    pub fn selection_info(&self) -> DeviceInfo {
        self.selection_info_with(self.udid())
    }

    fn selection_info_with(&self, udid: String) -> DeviceInfo {
        DeviceInfo {
            udid,
            name: rt::string_prop(&self.obj, sel!(name)),
            state: self.state(),
            runtime_id: String::new(),
            device_type_id: String::new(),
            available: false,
            platform: Platform::Ios,
            form_factor: None,
            sub_platform: None,
        }
    }

    /// A snapshot of the device's identity and state.
    pub fn info(&self) -> DeviceInfo {
        let d = &*self.obj;
        let udid = self.udid();
        let runtime_id = rt::string_prop(d, sel!(runtimeIdentifier));
        let device_type_id = rt::string_prop(d, sel!(deviceTypeIdentifier));
        let kind = apple_kind(&device_type_id, &runtime_id);
        DeviceInfo {
            udid,
            name: rt::string_prop(d, sel!(name)),
            state: self.state(),
            runtime_id,
            device_type_id,
            // SAFETY: `available` is a BOOL getter on SimDevice (guarded by the responds check).
            available: rt::responds(d, sel!(available)) && unsafe { msg_send![d, available] },
            platform: Platform::Ios,
            form_factor: None,
            sub_platform: None,
        }
        .with_kind(kind)
    }

    /// Current lifecycle state.
    pub fn state(&self) -> DeviceState {
        if !rt::responds(&self.obj, sel!(state)) {
            return DeviceState::Unknown;
        }
        // SAFETY: `state` returns an unsigned long long (checked to exist).
        DeviceState::from_raw(unsafe { msg_send![&*self.obj, state] })
    }

    /// When the device's current (or last) boot began, in seconds since the reference date; `None`
    /// when the service doesn't say. A reboot changes it.
    pub fn last_booted_at(&self) -> Option<f64> {
        let date = rt::optional_prop(&self.obj, sel!(lastBootedAt))?;
        // SAFETY: an NSDate's `timeIntervalSinceReferenceDate` takes no arguments and returns a double.
        unsafe { rt::send_scalar(&date, sel!(timeIntervalSinceReferenceDate)) }
    }

    /// Calls `on_change` whenever the device's state changes (booting, booted, shutting down,
    /// shut down), until the returned watch drops: the device's `device_state` notifications, the
    /// signal idb drops its HID connection on. `on_change` runs on a service queue: keep it short.
    pub fn watch_state(&self, on_change: impl Fn() + Send + Sync + 'static) -> Result<StateWatch> {
        let register = sel!(registerNotificationHandler:);
        rt::require(&self.obj, register)?;
        rt::require(&self.obj, sel!(unregisterNotificationHandler:error:))?;
        let block = RcBlock::with_encoding::<_, _, _, rt::ObjBlock>(move |info: *mut AnyObject| {
            // A notification that can't be read counts as a change: reconnecting is always safe.
            let state = crate::autoreleasepool(|| {
                // SAFETY: the service passes nil or a valid NSDictionary for the duration of the call.
                unsafe { info.as_ref() }.and_then(|d| rt::dict_string(d, "notification")).is_none_or(|n| n == "device_state")
            });
            if state {
                on_change();
            }
        });
        type RegisterFn = unsafe extern "C" fn(*const AnyObject, Sel, *const Block<dyn Fn(*mut AnyObject)>) -> u64;
        // SAFETY: the method's signature is (void (^)(NSDictionary *)) -> unsigned long long.
        let send: RegisterFn = unsafe { std::mem::transmute(objc2::ffi::objc_msgSend as unsafe extern "C-unwind" fn()) };
        // SAFETY: the device responds to the selector (checked); the service copies the block, and
        // the watch keeps ours alive until it unregisters.
        let handle = unsafe { send(&*self.obj, register, &*block) };
        Ok(StateWatch { device: self.obj.clone(), handle, _block: block })
    }

    /// Whether the device reports its boot sequence finished (data migration done, SpringBoard up).
    /// `None` when the service does not expose boot status.
    pub fn boot_finished(&self) -> Option<bool> {
        let info = rt::object_prop(&self.obj, sel!(bootStatus)).ok()?;
        if !rt::responds(&info, sel!(isTerminalStatus)) {
            return None;
        }
        type Getter = unsafe extern "C" fn(*const AnyObject, objc2::runtime::Sel) -> bool;
        // SAFETY: `isTerminalStatus` takes no arguments and returns BOOL (checked to exist).
        let send: Getter = unsafe { std::mem::transmute(objc2::ffi::objc_msgSend as unsafe extern "C-unwind" fn()) };
        // SAFETY: `info` is a valid object that responds to the selector.
        Some(unsafe { send(&*info, sel!(isTerminalStatus)) })
    }

    /// The device type's main screen size in pixels and its scale factor.
    pub fn device_type(&self) -> Result<DeviceTypeInfo> {
        Ok(device_type_info(&*rt::object_prop(&self.obj, sel!(deviceType))?))
    }

    /// Boots the device; returns once the service reports it booted.
    pub fn boot(&self) -> Result<()> {
        self.boot_with_env(&[])
    }

    /// Boots the device with extra environment for its launchd (`env` boot option).
    pub fn boot_with_env(&self, env: &[(String, String)]) -> Result<()> {
        self.boot_with(&BootOptions { env, jobs: &[] })
    }

    /// Boots the device with `opts` (see [`BootOptions`]).
    pub fn boot_with(&self, opts: &BootOptions<'_>) -> Result<()> {
        let options = boot_options(opts, &disabled_jobs_key());
        device_call!(self, "boot", bootWithOptions: &*options)
    }

    /// Shuts the device down.
    pub fn shutdown(&self) -> Result<()> {
        rt::require(&self.obj, sel!(shutdownWithError:))?;
        // SAFETY: the method exists (checked) and takes NSError**, returning BOOL.
        let done: std::result::Result<(), Retained<NSError>> = unsafe { msg_send![&*self.obj, shutdownWithError: _] };
        done.map_err(|e| rt::service_error("shutdown", &e))
    }

    /// Erases all content and settings. The device must be shut down.
    pub fn erase(&self) -> Result<()> {
        rt::require(&self.obj, sel!(eraseContentsAndSettingsWithError:))?;
        // SAFETY: the method exists (checked) and takes NSError**, returning BOOL.
        let done: std::result::Result<(), Retained<NSError>> = unsafe { msg_send![&*self.obj, eraseContentsAndSettingsWithError: _] };
        done.map_err(|e| rt::service_error("erase", &e))
    }

    /// Opens a URL on the device.
    pub fn open_url(&self, url: &str) -> Result<()> {
        // SAFETY: `+[NSURL URLWithString:]` takes an NSString and returns nil for malformed input.
        let parsed: Option<Obj> = unsafe { msg_send![objc2::class!(NSURL), URLWithString: &*rt::nsstring(url)] };
        let parsed = parsed.ok_or_else(|| Error::Invalid(format!("malformed URL: {url}")))?;
        device_call!(self, "open URL", openURL: &*parsed)
    }

    /// Installs the `.app` bundle at `path`.
    pub fn install(&self, path: &str) -> Result<()> {
        let absolute = std::path::absolute(path).map_err(|e| Error::Invalid(format!("{path}: {e}")))?;
        if !absolute.exists() {
            return Err(Error::Invalid(format!("no such app bundle: {}", absolute.display())));
        }
        device_call!(self, "install", installApplication: &*rt::file_url(&absolute.to_string_lossy()), withOptions: &*rt::empty_dictionary())
    }

    /// Uninstalls an app by bundle identifier.
    pub fn uninstall(&self, bundle_id: &str) -> Result<()> {
        device_call!(self, "uninstall", uninstallApplication: &*rt::nsstring(bundle_id), withOptions: &*rt::empty_dictionary())
    }

    /// Launches an app; returns its process id.
    pub fn launch(&self, bundle_id: &str) -> Result<i32> {
        rt::require(&self.obj, sel!(launchApplicationWithID:options:error:))?;
        let mut error: *mut NSError = ptr::null_mut();
        // SAFETY: the method exists (checked) and takes (NSString, NSDictionary, NSError**), returning int.
        let pid: i32 = unsafe {
            msg_send![&*self.obj, launchApplicationWithID: &*rt::nsstring(bundle_id), options: &*rt::empty_dictionary(), error: &mut error]
        };
        if pid > 0 { Ok(pid) } else { Err(launch_error(bundle_id, error)) }
    }

    /// Terminates an app by bundle identifier.
    pub fn terminate(&self, bundle_id: &str) -> Result<()> {
        device_call!(self, "terminate", terminateApplicationWithID: &*rt::nsstring(bundle_id))
    }

    /// Bundle identifiers of installed apps.
    pub fn installed_apps(&self) -> Result<Vec<String>> {
        rt::require(&self.obj, sel!(installedAppsWithError:))?;
        // SAFETY: the method exists (checked) and takes NSError**, returning an NSDictionary keyed by bundle id.
        let apps: std::result::Result<Obj, Retained<NSError>> = unsafe { msg_send![&*self.obj, installedAppsWithError: _] };
        let apps = apps.map_err(|e| rt::service_error("list apps", &e))?;
        // SAFETY: NSDictionary responds to `allKeys`, returning an NSArray.
        let keys: Obj = unsafe { msg_send![&*apps, allKeys] };
        let mut ids: Vec<String> = rt::array_items(&keys).iter().map(|k| rt::string_prop(k, sel!(description))).collect();
        ids.sort();
        Ok(ids)
    }

    /// Switches between light and dark UI.
    pub fn set_appearance(&self, appearance: Appearance) -> Result<()> {
        let style: isize = match appearance {
            Appearance::Light => 1,
            Appearance::Dark => 2,
        };
        device_call!(self, "set appearance", setUIInterfaceStyle: style)
    }

    /// Sets the simulated location.
    pub fn set_location(&self, latitude: f64, longitude: f64) -> Result<()> {
        device_call!(self, "set location", setLocationWithLatitude: latitude, andLongitude: longitude)
    }

    /// Grants or revokes a privacy permission (`all`, `photos`, `camera`, ...) for an app.
    pub fn set_privacy(&self, service: &str, bundle_id: &str, granted: bool) -> Result<()> {
        device_call!(self, "set privacy", setPrivacyAccessForService: &*rt::nsstring(service), bundleID: &*rt::nsstring(bundle_id), granted: granted)
    }

    /// Posts a Darwin notification inside the device.
    pub fn post_notification(&self, name: &str) -> Result<()> {
        device_call!(self, "post notification", postDarwinNotification: &*rt::nsstring(name))
    }

    /// Triggers a simulated memory warning.
    pub fn memory_warning(&self) -> Result<()> {
        rt::require(&self.obj, sel!(simulateMemoryWarningWithError:))?;
        // SAFETY: the method exists (checked) and takes NSError**, returning BOOL.
        let done: std::result::Result<(), Retained<NSError>> = unsafe { msg_send![&*self.obj, simulateMemoryWarningWithError: _] };
        done.map_err(|e| rt::service_error("memory warning", &e))
    }

    /// The device's data directory on the host (its whole filesystem lives under it).
    pub fn data_path(&self) -> String {
        rt::string_prop(&self.obj, sel!(dataPath))
    }

    /// The host path of the device's OS runtime root (the `/` that in-device programs see).
    pub fn runtime_root(&self) -> Result<String> {
        let runtime = rt::object_prop(&self.obj, sel!(runtime))?;
        let root = rt::string_prop(&runtime, sel!(root));
        if root.is_empty() { Err(Error::Unavailable("runtime root".into())) } else { Ok(root) }
    }

    /// Overrides status bar items. `None` leaves an item alone.
    pub fn override_status_bar(&self, o: &StatusBar) -> Result<()> {
        if let Some(time) = &o.time {
            device_call!(self, "status bar time", overrideStatusBarTimeString: &*rt::nsstring(time))?;
        }
        if let Some(net) = o.data_network {
            device_call!(self, "status bar network", overrideStatusBarDataNetworkType: net)?;
        }
        if let Some((mode, bars)) = o.wifi {
            device_call!(self, "status bar wifi", overrideStatusBarWiFiMode: mode, bars: bars)?;
        }
        if let Some((mode, bars)) = o.cellular {
            let operator = o.operator.clone().unwrap_or_default();
            device_call!(self, "status bar cellular", overrideStatusBarCellularMode: mode, operatorName: &*rt::nsstring(&operator), bars: bars)?;
        }
        if let Some((state, level)) = o.battery {
            device_call!(self, "status bar battery", overrideStatusBarBatteryState: state, batteryLevel: level, showNotCharging: false)?;
        }
        Ok(())
    }

    /// Removes every status bar override.
    pub fn clear_status_bar(&self) -> Result<()> {
        device_call!(self, "clear status bar", clearStatusBarOverrides: u64::MAX)
    }

    /// Delivers a push notification payload (APNs JSON) to an app.
    pub fn push(&self, bundle_id: &str, json: &str) -> Result<()> {
        let payload = rt::json_object(json)?;
        device_call!(self, "push", sendPushNotificationForBundleID: &*rt::nsstring(bundle_id), jsonPayload: &*payload)
    }

    /// Deletes every keychain item.
    pub fn reset_keychain(&self) -> Result<()> {
        rt::require(&self.obj, sel!(resetKeychainWithError:))?;
        // SAFETY: the method exists (checked) and takes NSError**, returning BOOL.
        let done: std::result::Result<(), Retained<NSError>> = unsafe { msg_send![&*self.obj, resetKeychainWithError: _] };
        done.map_err(|e| rt::service_error("reset keychain", &e))
    }

    /// Sets the Dynamic Type size (0 = extra small … 6 = XXXL, 7… = accessibility sizes).
    pub fn set_content_size(&self, category: i64) -> Result<()> {
        device_call!(self, "content size", setContentSizeCategory: category as isize)
    }

    /// Turns Increase Contrast on or off.
    pub fn set_increase_contrast(&self, on: bool) -> Result<()> {
        device_call!(self, "increase contrast", setIncreaseContrastEnabled: on)
    }

    /// Hides or shows the Dynamic Island.
    pub fn set_dynamic_island_suppressed(&self, on: bool) -> Result<()> {
        device_call!(self, "dynamic island", setDynamicIslandSuppressed: on)
    }

    /// Turns the display backlight on or off.
    pub fn set_backlight(&self, on: bool) -> Result<()> {
        device_call!(self, "backlight", setDisplayBacklightActive: on)
    }

    /// Sets the software keyboard language (e.g. `en_US`).
    pub fn set_keyboard_language(&self, language: &str) -> Result<()> {
        device_call!(self, "keyboard language", setKeyboardLanguage: &*rt::nsstring(language))
    }

    /// Connects or disconnects the hardware keyboard (when disconnected the software keyboard shows).
    pub fn set_hardware_keyboard(&self, on: bool) -> Result<()> {
        device_call!(self, "hardware keyboard", setHardwareKeyboardEnabled: on, keyboardType: 0u8)
    }

    /// Reads a Core Animation render-server debug option.
    pub fn ca_debug(&self, option: u32) -> bool {
        if !rt::responds(&self.obj, sel!(getCADebugOption:)) {
            return false;
        }
        // SAFETY: `getCADebugOption:` takes an unsigned int and returns BOOL (checked to exist).
        unsafe { msg_send![&*self.obj, getCADebugOption: option] }
    }

    /// Sets a Core Animation render-server debug option (see `sim_core::debug::CA_OPTIONS`).
    pub fn set_ca_debug(&self, option: u32, on: bool) -> Result<()> {
        rt::require(&self.obj, sel!(setCADebugOption:enabled:))?;
        // SAFETY: the method exists (checked) and takes (unsigned int, BOOL), returning void.
        let _: () = unsafe { msg_send![&*self.obj, setCADebugOption: option, enabled: on] };
        Ok(())
    }

    /// Sets a Darwin notification's state value inside the device.
    pub fn set_notification_state(&self, name: &str, state: u64) -> Result<()> {
        device_call!(self, "set notification state", darwinNotificationSetState: state, name: &*rt::nsstring(name))
    }

    /// Spawns `path` inside the device with `args` (argv\[1..]) and `env`, wiring stdout/stderr to
    /// host file descriptors. Returns the pid. The child is not waited for: read its output pipe to EOF.
    pub fn spawn(&self, path: &str, args: &[String], env: &[(String, String)], stdout: Option<i32>, stderr: Option<i32>) -> Result<i32> {
        rt::require(&self.obj, sel!(spawnWithPath:options:terminationHandler:error:))?;
        let options = rt::mutable_dictionary();
        let mut argv = vec![path.to_owned()];
        argv.extend_from_slice(args);
        rt::dict_set(&options, "arguments", &rt::string_array(&argv));
        rt::dict_set(&options, "environment", &rt::string_dictionary(env));
        if let Some(fd) = stdout {
            rt::dict_set(&options, "stdout", &rt::nsnumber(i64::from(fd)));
        }
        if let Some(fd) = stderr {
            rt::dict_set(&options, "stderr", &rt::nsnumber(i64::from(fd)));
        }
        type Spawn = unsafe extern "C" fn(
            *const AnyObject,
            objc2::runtime::Sel,
            *const AnyObject,
            *const AnyObject,
            *const std::ffi::c_void,
            *mut *mut NSError,
        ) -> i32;
        // SAFETY: `objc_msgSend` is called through the method's signature
        // (NSString, NSDictionary, block, NSError**) -> int; a nil termination block is accepted.
        let send: Spawn = unsafe { std::mem::transmute(objc2::ffi::objc_msgSend as unsafe extern "C-unwind" fn()) };
        let mut error: *mut NSError = ptr::null_mut();
        let path = rt::nsstring(path);
        // SAFETY: all object arguments are live; `error` is a valid out-pointer.
        let pid = unsafe {
            send(
                &*self.obj,
                sel!(spawnWithPath:options:terminationHandler:error:),
                (&*path as &NSString as *const NSString).cast(),
                &*options,
                ptr::null(),
                &mut error,
            )
        };
        if pid > 0 { Ok(pid) } else { Err(rt::raw_error("spawn", error)) }
    }

    /// Runs `path` inside the device and collects its output. Resolves bare names against the
    /// runtime's `/usr/bin`, `/bin` and `/usr/sbin`.
    pub fn exec(&self, path: &str, args: &[String], env: &[(String, String)]) -> Result<ExecOutput> {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        let candidates = self.program(path)?;
        let (mut out_r, out_w) = std::io::pipe().map_err(|e| Error::Invalid(format!("pipe: {e}")))?;
        let (mut err_r, err_w) = std::io::pipe().map_err(|e| Error::Invalid(format!("pipe: {e}")))?;
        let mut result = Err(Error::NotFound(path.to_owned()));
        for candidate in &candidates {
            result = self.spawn(candidate, args, env, Some(out_w.as_raw_fd()), Some(err_w.as_raw_fd()));
            if result.is_ok() {
                break;
            }
        }
        drop((out_w, err_w));
        let pid = result?;
        let err_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            err_r.read_to_end(&mut buf).ok();
            buf
        });
        let mut stdout = Vec::new();
        out_r.read_to_end(&mut stdout).map_err(|e| Error::Invalid(format!("read: {e}")))?;
        let stderr = err_thread.join().unwrap_or_default();
        Ok(ExecOutput { pid, stdout: String::from_utf8_lossy(&stdout).into_owned(), stderr: String::from_utf8_lossy(&stderr).into_owned() })
    }

    /// Starts `path` inside the device, resolved as [`Device::exec`] does, and returns its pid and
    /// the read end of its stdout (its stderr isn't kept). The program runs until it exits or is
    /// signalled; the reader sees EOF then.
    pub fn exec_stream(&self, path: &str, args: &[String], env: &[(String, String)]) -> Result<(i32, std::io::PipeReader)> {
        use std::os::fd::AsRawFd;
        let candidates = self.program(path)?;
        let (out_r, out_w) = std::io::pipe().map_err(|e| Error::Invalid(format!("pipe: {e}")))?;
        let mut result = Err(Error::NotFound(path.to_owned()));
        for candidate in &candidates {
            result = self.spawn(candidate, args, env, Some(out_w.as_raw_fd()), None);
            if result.is_ok() {
                break;
            }
        }
        drop(out_w);
        Ok((result?, out_r))
    }

    /// The host paths `path` may run from, best first: under the runtime root (bare names resolve
    /// against the runtime's `/usr/bin`, `/bin`, `/usr/sbin`, `/sbin` and `/usr/libexec`), or a
    /// simulator binary built on the host from its own path.
    fn program(&self, path: &str) -> Result<Vec<String>> {
        // Spawned programs resolve paths on the host: prefix the runtime root, as the device sees it.
        let root = self.runtime_root()?;
        let inside: Vec<String> = if path.starts_with('/') {
            vec![path.to_owned()]
        } else {
            ["/usr/bin", "/bin", "/usr/sbin", "/sbin", "/usr/libexec"].iter().map(|d| format!("{d}/{path}")).collect()
        };
        let mut candidates: Vec<String> =
            inside.iter().map(|p| format!("{root}{p}")).filter(|p| std::path::Path::new(p).exists()).collect();
        // A simulator binary built on the host (e.g. a test tool) runs from its host path.
        if candidates.is_empty() && path.starts_with('/') && std::path::Path::new(path).is_file() {
            candidates.push(path.to_owned());
        }
        if candidates.is_empty() {
            return Err(Error::NotFound(format!("{path} in the device runtime")));
        }
        Ok(candidates)
    }

    /// Launches an app with launch arguments and environment variables. Returns its pid.
    /// `restart`: a running instance is terminated first and the app cold-starts; otherwise
    /// the service's `terminate_running_process` is false and a running app is left as it is.
    pub fn launch_with(&self, bundle_id: &str, args: &[String], env: &[(String, String)], restart: bool) -> Result<i32> {
        rt::require(&self.obj, sel!(launchApplicationWithID:options:error:))?;
        let options = rt::mutable_dictionary();
        rt::dict_set(&options, "arguments", &rt::string_array(args));
        rt::dict_set(&options, "environment", &rt::string_dictionary(env));
        rt::dict_set(&options, "terminate_running_process", &rt::nsbool(restart));
        let mut error: *mut NSError = ptr::null_mut();
        // SAFETY: the method exists (checked) and takes (NSString, NSDictionary, NSError**), returning int.
        let pid: i32 =
            unsafe { msg_send![&*self.obj, launchApplicationWithID: &*rt::nsstring(bundle_id), options: &*options, error: &mut error] };
        if pid > 0 { Ok(pid) } else { Err(launch_error(bundle_id, error)) }
    }

    /// The device's main display. The device must be booted.
    pub fn display(&self) -> Result<Display> {
        Display::main(&self.obj, self.main_screen())
    }

    /// The device's built-in displays: the main one and, on a device with two built-in screens
    /// (the iPhone Duo), the other (inner) one. The device must be booted.
    pub fn builtin_displays(&self) -> Result<(Display, Option<Display>)> {
        Display::builtins(&self.obj, self.main_screen())
    }

    /// The device type's main screen in pixels, when the service says.
    fn main_screen(&self) -> Option<(u32, u32)> {
        let ty = rt::optional_prop(&self.obj, sel!(deviceType))?;
        let info = device_type_info(&ty);
        let (w, h) = (info.screen_width.round(), info.screen_height.round());
        (w >= 1.0 && h >= 1.0).then_some((w as u32, h as u32))
    }

    /// The external display (an iPhone's CarPlay screen once attached), not the main one.
    pub fn external_display(&self) -> Result<Display> {
        Display::external(&self.obj)
    }

    /// A HID input session. The device must be booted. Buttons and form-factor inputs follow the
    /// device's form factor (the Siri Remote on Apple TV, the crown on Apple Watch).
    pub fn hid(&self) -> Result<Hid> {
        // The scale turns the pointer's pixels into the points it moves in; 0 where unknown.
        let scale = self.device_type().map_or(0.0, |t| t.screen_scale);
        Hid::connect(&self.obj, self.kit, self.info().form_factor, scale)
    }

    /// Attaches (or detaches) the CarPlay display to this iPhone and creates (or removes) the
    /// CarPlay HID service its touches go to. iOS 26 runtimes and earlier: iOS 27 renders it black.
    pub fn set_carplay(&self, on: bool) -> Result<()> {
        let info = self.info();
        use sim_core::form_factor::FormFactor;
        if !matches!(info.form_factor, Some(FormFactor::Phone | FormFactor::Foldable)) {
            return Err(Error::Invalid(format!("CarPlay attaches to an iPhone; {} is {}", info.name, info.label())));
        }
        sim_core::carplay::runtime_supports(&info.runtime_id).map_err(Error::Invalid)?;
        let hid = self.hid()?;
        if on {
            crate::carplay::set_display(&self.obj, &self.data_path(), true)?;
            hid.carplay_service(true)
        } else {
            // The service may already be gone (a reboot removes it); detaching goes on regardless.
            let removed = hid.carplay_service(false);
            crate::carplay::set_display(&self.obj, &self.data_path(), false)?;
            removed
        }
    }

    /// Rotates the device's interface to `orientation`: a GSEvent to SpringBoard's
    /// PurpleWorkspacePort, what Simulator.app's Rotate Left/Right send.
    pub fn set_orientation(&self, orientation: Orientation) -> Result<()> {
        use sim_core::form_factor::FormFactor;
        let info = self.info();
        // Only SpringBoard takes the message; elsewhere (Apple TV's PineBoard) it's silently dropped.
        if !matches!(info.form_factor, Some(FormFactor::Phone | FormFactor::Tablet | FormFactor::Foldable) | None) {
            return Err(Error::Invalid(format!("only an iPhone or iPad rotates; {} is {}", info.name, info.label())));
        }
        let sel = sel!(lookup:error:);
        rt::require(&self.obj, sel)?;
        let mut error: *mut NSError = std::ptr::null_mut();
        // SAFETY: `lookup:error:` takes (NSString, NSError**) and returns a mach_port_t (0 on failure).
        let port: u32 = unsafe { msg_send![&*self.obj, lookup: &*rt::nsstring("PurpleWorkspacePort"), error: &mut error] };
        if port == 0 {
            return Err(rt::raw_error("look up PurpleWorkspacePort", error));
        }
        let mut message = orientation.purple_message(port);
        // SAFETY: `message` is a complete 108-byte mach message (header included) in a 112-byte
        // buffer; a send-only call with a timeout doesn't touch receive arguments.
        let kr = unsafe { mach_msg(message.as_mut_ptr().cast(), MACH_SEND_MSG | MACH_SEND_TIMEOUT, 108, 0, 0, 2000, 0) };
        // SAFETY: the lookup gave this task a send right to `port`; release it.
        unsafe { mach_port_deallocate(mach_task_self_, port) };
        if kr != 0 {
            return Err(Error::Service { op: "rotate", message: format!("mach_msg to PurpleWorkspacePort failed ({kr:#x})") });
        }
        Ok(())
    }
}
