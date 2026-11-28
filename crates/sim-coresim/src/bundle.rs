//! The simulator service as `sim-bundle`'s iOS [`Service`] (#421): export and import read and
//! make devices through it in process, and do the rest (walking, chunking, checking) themselves.

use sim_bundle::ios::{Runtime, Service, Simulator};
use sim_core::device::DeviceState;

use crate::{Context, Device};

/// A [`Context`]'s device set as a bundle [`Service`].
pub struct Bundles<'a>(pub &'a Context);

fn simulator(d: &Device) -> Simulator {
    let info = d.info();
    Simulator {
        shut_down: info.state == DeviceState::Shutdown,
        state: info.state.as_str().to_owned(),
        form_factor: info.form_factor.map(|f| f.as_str().to_owned()),
        udid: info.udid,
        name: info.name,
        device_type: info.device_type_id,
        runtime: info.runtime_id,
        data_dir: d.data_path().into(),
    }
}

impl Service for Bundles<'_> {
    fn device(&self, udid: &str) -> Result<Simulator, String> {
        self.0.device(udid).map(|d| simulator(&d)).map_err(|e| e.to_string())
    }

    fn names(&self) -> Result<Vec<String>, String> {
        Ok(self.0.selection_infos().map_err(|e| e.to_string())?.into_iter().map(|i| i.name).collect())
    }

    fn runtimes(&self) -> Result<Vec<Runtime>, String> {
        let list = self.0.runtimes().map_err(|e| e.to_string())?;
        Ok(list
            .into_iter()
            .map(|r| Runtime { identifier: r.identifier, name: r.name, version: r.version, build: r.build, available: r.available })
            .collect())
    }

    fn create(&self, name: &str, device_type: &str, runtime: &Runtime) -> Result<Simulator, String> {
        if !self.0.device_types().map_err(|e| e.to_string())?.iter().any(|t| t.identifier.eq_ignore_ascii_case(device_type)) {
            return Err(format!("this host has no device type {device_type} (the Xcode that made the bundle's may)"));
        }
        self.0.create_on_build(name, device_type, &runtime.identifier, &runtime.build).map(|d| simulator(&d)).map_err(|e| e.to_string())
    }

    fn delete(&self, udid: &str) -> Result<(), String> {
        let d = self.0.device(udid).map_err(|e| e.to_string())?;
        self.0.delete(&d).map_err(|e| e.to_string())
    }
}
