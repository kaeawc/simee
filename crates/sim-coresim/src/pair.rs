//! Watch-phone device pairs (#423): CoreSimulator's `SimDevicePair`s in a device set, listed,
//! checked, made, made active and removed in-process, as `simctl list pairs`, `pair`,
//! `pair_activate` and `unpair` do. The pairs live in the set's `device_set.plist`, so they outlive
//! this process and a host reboot.

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::sel;
use objc2_foundation::NSError;
use sim_core::link::Pair;

use crate::error::{Error, Result};
use crate::rt::{self, Obj};
use crate::service::{Context, Device};

/// A device's UDID, read as [`Device::udid`] does.
fn udid_of(device: &AnyObject) -> String {
    rt::object_prop(device, sel!(UDID)).map(|u| rt::string_prop(&u, sel!(UUIDString))).unwrap_or_default()
}

/// A BOOL property, `false` where the object doesn't have it.
fn flag(obj: &AnyObject, sel: objc2::runtime::Sel) -> bool {
    // SAFETY: callers pass BOOL getters taking no arguments; `send_scalar` checks the object responds.
    unsafe { rt::send_scalar::<bool>(obj, sel) }.unwrap_or(false)
}

/// What a `SimDevicePair` says: its UUID, the watch (`gizmo`) and the phone (`companion`), and
/// whether it is active and connected.
fn pair_of(obj: &AnyObject) -> Pair {
    let device = |sel| rt::optional_prop(obj, sel).map(|d| udid_of(&d)).unwrap_or_default();
    Pair {
        id: rt::object_prop(obj, sel!(UUID)).map(|u| rt::string_prop(&u, sel!(UUIDString))).unwrap_or_default(),
        watch: device(sel!(gizmo)),
        phone: device(sel!(companion)),
        active: flag(obj, sel!(active)),
        connected: flag(obj, sel!(connected)),
    }
}

impl Context {
    /// The set's `SimDevicePair` objects: `devicePairs` (an array, or a dictionary's values).
    fn pair_objects(&self) -> Result<Vec<Obj>> {
        let list = rt::object_prop(&self.set, sel!(devicePairs))?;
        let list: Obj = if rt::responds(&list, sel!(allValues)) {
            // SAFETY: `allValues` on an NSDictionary returns a non-nil NSArray.
            unsafe { msg_send![&*list, allValues] }
        } else {
            list
        };
        Ok(rt::array_items(&list))
    }

    /// The pair whose UUID is `id` (in any case).
    fn pair_object(&self, id: &str) -> Result<Obj> {
        self.pair_objects()?.into_iter().find(|p| pair_of(p).id.eq_ignore_ascii_case(id)).ok_or_else(|| Error::NotFound(id.to_owned()))
    }

    /// Every watch-phone pair in the set (`simctl list pairs`).
    pub fn device_pairs(&self) -> Result<Vec<Pair>> {
        let mut pairs: Vec<Pair> = self.pair_objects()?.iter().map(|p| pair_of(p)).collect();
        pairs.sort_by(|a, b| (&a.phone, &a.watch).cmp(&(&b.phone, &b.watch)));
        Ok(pairs)
    }

    /// Whether CoreSimulator would pair `watch` with `phone` (their device types and runtimes, the
    /// phone's pair limit), without pairing them: its refusal as the error.
    pub fn test_pairing(&self, watch: &Device, phone: &Device) -> Result<()> {
        rt::require(&self.set, sel!(testDevicePairingBetweenGizmo:companion:error:))?;
        // SAFETY: the method exists (checked) and takes (SimDevice, SimDevice, NSError**), returning
        // BOOL with the error set when it is NO.
        let ok: std::result::Result<(), Retained<NSError>> =
            unsafe { msg_send![&*self.set, testDevicePairingBetweenGizmo: &*watch.obj, companion: &*phone.obj, error: _] };
        ok.map_err(|e| rt::service_error("pair", &e))
    }

    /// Pairs `watch` with `phone` (`simctl pair`): the new pair.
    pub fn create_pair(&self, watch: &Device, phone: &Device) -> Result<Pair> {
        rt::require(&self.set, sel!(createDevicePairWithGizmo:companion:error:))?;
        // SAFETY: the method exists (checked) and takes (SimDevice, SimDevice, NSError**), returning
        // the new SimDevicePair (nil with the error set on failure).
        let made: std::result::Result<Obj, Retained<NSError>> =
            unsafe { msg_send![&*self.set, createDevicePairWithGizmo: &*watch.obj, companion: &*phone.obj, error: _] };
        let made = made.map_err(|e| rt::service_error("pair", &e))?;
        Ok(pair_of(&made))
    }

    /// Makes the pair `id` its phone's active one (`simctl pair_activate`).
    pub fn activate_pair(&self, id: &str) -> Result<()> {
        let pair = self.pair_object(id)?;
        rt::require(&pair, sel!(setActiveWithError:))?;
        // SAFETY: the method exists (checked) and takes NSError**, returning BOOL.
        let done: std::result::Result<(), Retained<NSError>> = unsafe { msg_send![&*pair, setActiveWithError: _] };
        done.map_err(|e| rt::service_error("pair_activate", &e))
    }

    /// Removes the pair `id` (`simctl unpair`).
    pub fn unpair(&self, id: &str) -> Result<()> {
        let pair = self.pair_object(id)?;
        rt::require(&self.set, sel!(unpairDevicePair:error:))?;
        // SAFETY: the method exists (checked) and takes (SimDevicePair, NSError**), returning BOOL.
        let done: std::result::Result<(), Retained<NSError>> = unsafe { msg_send![&*self.set, unpairDevicePair: &*pair, error: _] };
        done.map_err(|e| rt::service_error("unpair", &e))
    }
}
