//! Serves emu's device set for an Android guest whose CPU runs in QEMU (software mode on Linux,
//! #55), one socket per device, and prints the QEMU arguments that attach them. A prototype of
//! what `emu` will run itself:
//! - over vhost-user: vsock (adb and the qemud pipes), the virtio-console ports (the modem, logcat,
//!   uwb, bluetooth), virtio-net behind emu's user-mode NAT, and with `EMU_LIB64=<renderer lib64>`
//!   the GPU (gfxstream on SwiftShader, Vulkan only, blobs exported as fds QEMU maps);
//! - over vfio-user, with the GPU: the goldfish address space (the ranchu gralloc's host memory)
//!   and the goldfish pipe (the graphics pipes: `refcount`, `GLProcessPipe`), the pipe in a PCI
//!   function at slot 0x1e that an ACPI table (`goldfish-pipe.aml`, for `-acpitable`) describes as
//!   `GFSH0003`.
//!
//! ```text
//! cargo run --release -p simee-emu-vmm --example soft_devices -- <socket dir> <adb port> [prop=value ...]
//! ```
#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::path::PathBuf;
    use std::sync::Arc;

    use simee_emu_vmm::devices::goldfish_pipe::{GlProcess, RefCount};
    use simee_emu_vmm::pipe::{PipeRegistry, adb::AdbService, misc::MiscService, qemud::BootProperties, sensors};
    use simee_emu_vmm::vhost_user::BackendChannel;
    use simee_emu_vmm::virtio::console::{Console, PortSpec, Sink};
    use simee_emu_vmm::virtio::vsock::{Vsock, event_channel};
    use simee_emu_vmm::virtio::{VirtioDevice, net};

    const PIPE_SLOT: u8 = 0x1e;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(dir), Some(port)) = (args.first().map(PathBuf::from), args.get(1).and_then(|p| p.parse::<u16>().ok())) else {
        return Err("usage: soft_devices <socket dir> <adb port> [prop=value ...]".into());
    };
    std::fs::create_dir_all(&dir)?;
    let props = args[2..].iter().filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let mut qemu: Vec<String> = Vec::new();
    let mut servers = Vec::new();

    // The goldfish address space and the renderer first: the pipe registry's `refcount` frees
    // color buffers in the renderer, and the renderer's DMA reaches into the area.
    let lib64 = std::env::var_os("EMU_LIB64").map(PathBuf::from);
    let gas = simee_emu_vmm::vfio_user::GoldfishAs::new(16 << 30)?;
    let (area, ram) = (gas.area(), gas.guest_ram_handle());
    let gpu = match &lib64 {
        Some(lib64) => Some(gpu(lib64, area, ram)?),
        None => None,
    };

    let (events, rx) = event_channel();
    let adb = AdbService::bind(port, events.clone())?;
    let mut registry = PipeRegistry::default();
    registry.register("qemud:adb", Box::new(adb));
    registry.register("qemud:boot-properties", Box::new(BootProperties { props }));
    registry.register("qemud:sensors", Box::new(sensors::SensorService { sensors: Arc::new(sensors::Sensors::default()) }));
    registry.register("QemuMiscPipe", Box::new(MiscService { on_event: None }));
    // What emu's own machine serves (crates/emu-vmm/src/machine.rs): a HAL whose pipe isn't
    // served retries it in a loop (the camera and fingerprint HALs took half a core each).
    registry.register("clipboard", Box::new(simee_emu_vmm::pipe::clipboard::ClipboardService { clipboard: Arc::default() }));
    registry.register(
        "qemud:fingerprintlisten",
        Box::new(simee_emu_vmm::pipe::fingerprint::FingerprintService { fingerprint: Arc::default() }),
    );
    registry.register("qemud:gps", Box::new(simee_emu_vmm::pipe::gps::GpsService { gps: Arc::default() }));
    registry.register("multidisplay", Box::new(simee_emu_vmm::pipe::misc::Silent("multidisplay")));
    registry.register("qemud:camera", Box::new(simee_emu_vmm::pipe::camera::CameraService::new(None)));
    registry.register("GLProcessPipe", Box::new(GlProcess::default()));
    let on_last_ref = gpu.as_ref().map(|(_, _, _, r)| -> simee_emu_vmm::devices::goldfish_pipe::OnLastRef {
        let r = r.clone();
        Arc::new(move |handle| {
            let _ = r.release_color_buffer(handle);
        })
    });
    registry.register("refcount", Box::new(RefCount { on_last_ref }));
    let registry = Arc::new(registry);
    let vsock = Vsock::new(3, registry.clone(), events, rx);
    let vsock_path = dir.join("vsock.sock");
    qemu.push(format!("-chardev socket,id=vsock,path={}", vsock_path.display()));
    qemu.push("-device vhost-user-vsock-pci,chardev=vsock".into());
    servers.push(simee_emu_vmm::vhost_user::serve(&vsock_path, "vsock", Box::new(vsock))?);

    let telephony = Arc::new(simee_emu_vmm::telephony::Telephony::default());
    let (console, handles) = Console::new(vec![
        PortSpec { name: String::new(), backend: Box::new(Sink) },
        PortSpec { name: "modem".into(), backend: telephony.backend() },
        PortSpec { name: String::new(), backend: Box::new(Sink) },
        PortSpec { name: "uwb".into(), backend: Box::new(simee_emu_vmm::bt::UwbPort::default()) },
        PortSpec {
            name: "bluetooth".into(),
            backend: Box::new(simee_emu_vmm::bt::BluetoothPort::new(simee_emu_vmm::bt::address_for(b"soft"))),
        },
    ]);
    if let Some(h) = handles.get(1) {
        telephony.attach(h.clone());
    }
    let nat: Box<dyn net::Backend> = match simee_emu_vmm::net::Nat::new(simee_emu_vmm::net::Config::default()) {
        Ok(n) => Box::new(n),
        Err(e) => {
            eprintln!("no NAT ({e}): link only");
            Box::new(net::LinkOnly)
        }
    };

    // (name, device, backend channel); QEMU's generic frontend takes the id, queue count and
    // config size.
    let mut devices: Vec<(&str, Box<dyn VirtioDevice>, BackendChannel)> =
        vec![("console", Box::new(console), Default::default()), ("net", Box::new(net::Net::new(nat)), Default::default())];
    if let Some((dev, chan, frames, _)) = gpu {
        devices.push(("gpu", dev, chan));
        // Every 10 s: the frame count, and the latest frame as a PNG (the host's view, as emu's
        // screenshots are taken).
        let shot = dir.join("screen.png");
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(10));
                println!("frames presented: {}", frames.frames());
                let png = frames.latest(|c| {
                    sim_core::frame::encode_png(&sim_core::frame::Frame { width: c.width, height: c.height, rgba: c.rgba.clone() })
                });
                if let Some(Ok(png)) = png {
                    let _ = std::fs::write(&shot, png);
                }
            }
        });
        let gas_path = dir.join("goldfish-as.sock");
        servers.push(simee_emu_vmm::vfio_user::serve(&gas_path, "goldfish-as", Box::new(gas))?);
        qemu.push(format!("-device '{}'", simee_emu_vmm::vfio_user::qemu_device(&gas_path, None)));
        let pipe_path = dir.join("goldfish-pipe.sock");
        let carrier = simee_emu_vmm::vfio_user::GoldfishPipeCarrier::new(registry.clone());
        servers.push(simee_emu_vmm::vfio_user::serve(&pipe_path, "goldfish-pipe", Box::new(carrier))?);
        qemu.push(format!("-device '{}'", simee_emu_vmm::vfio_user::qemu_device(&pipe_path, Some(PIPE_SLOT))));
        let aml = dir.join("goldfish-pipe.aml");
        std::fs::write(&aml, simee_emu_vmm::acpi::goldfish_pipe_ssdt(PIPE_SLOT, simee_emu_vmm::layout::GOLDFISH_PIPE_SIZE as u32))?;
        qemu.push(format!("-acpitable file={}", aml.display()));
        // The 64-bit window has to hold the GPU's 8 GiB region and the 16 GiB address space.
        qemu.push("-global i440FX-pcihost.pci-hole64-size=64G".into());
    }
    for (name, dev, chan) in devices {
        let (id, queues, config) = (dev.device_id(), dev.num_queues(), config_size(dev.as_ref()));
        let path = dir.join(format!("{name}.sock"));
        qemu.push(format!("-chardev socket,id={name},path={}", path.display()));
        qemu.push(format!("-device vhost-user-test-device-pci,chardev={name},virtio-id={id},num_vqs={queues},config_size={config}"));
        servers.push(simee_emu_vmm::vhost_user::serve_with(&path, name, dev, chan)?);
    }
    std::fs::write(dir.join("qemu.args"), qemu.join(" "))?;
    println!("adb on 127.0.0.1:{port}; QEMU arguments (also in {}):\n{}", dir.join("qemu.args").display(), qemu.join(" "));
    AdbService::announce_port(port);
    for s in servers {
        let _ = s.join();
    }
    Ok(())
}

/// The GPU: the renderer from `lib64` on software Vulkan, Vulkan only, with blobs it can export, the
/// features emu's own machine uses, and its DMA reaching guest RAM and the address-space area.
/// `EMU_RENDERER_FEATURES` adds overrides.
#[cfg(target_os = "linux")]
#[allow(clippy::type_complexity)]
fn gpu(
    lib64: &std::path::Path,
    area: simee_emu_vmm::vfio_user::AreaHandle,
    ram: simee_emu_vmm::vfio_user::AreaHandle,
) -> Result<
    (
        Box<dyn simee_emu_vmm::virtio::VirtioDevice>,
        simee_emu_vmm::vhost_user::BackendChannel,
        std::sync::Arc<simee_emu_vmm::display::FrameStore>,
        std::sync::Arc<emu_gfxstream::Renderer>,
    ),
    Box<dyn std::error::Error>,
> {
    use std::sync::Arc;
    use std::time::Duration;

    use emu_gfxstream::{Icd, Renderer, flags};
    use simee_emu_vmm::display::{DisplaySink, FrameStore, PostTicker};
    use simee_emu_vmm::virtio::gpu::{Gpu, GpuConfig, fence_handler};

    let (w, h) = (720, 1280);
    // SwiftShader: the SDK's lavapipe crashes in a render thread minutes into a boot, and
    // SwiftShader booted faster (docs/android.md). `EMU_VK_ICD=lavapipe` (or gfxstream's own
    // `ANDROID_EMU_VK_ICD=lavapipe`, which scripts/tcg-bench.py sets) picks lavapipe.
    let lavapipe = ["EMU_VK_ICD", "ANDROID_EMU_VK_ICD"].iter().any(|k| std::env::var(k).is_ok_and(|v| v == "lavapipe"));
    let icd = if lavapipe { Icd::Lavapipe } else { Icd::SwiftShader };
    for n in emu_gfxstream::configure_environment(lib64.parent().ok_or("lib64 has no parent")?, icd) {
        eprintln!("{n}");
    }
    let r = Arc::new(Renderer::load(lib64)?);
    let store = Arc::new(FrameStore::with_idle(Duration::from_secs(5)));
    let ticker = PostTicker::start(store.clone(), w, h, Duration::from_millis(16), Duration::from_millis(50));
    let fences = fence_handler();
    let on_fence: Arc<dyn Fn(emu_gfxstream::Fence) + Send + Sync> = Arc::new(move |f| {
        fences(f);
        ticker.poke();
    });
    let mut features =
        String::from("RefCountPipe:enabled,GlDirectMem:enabled,HasSharedSlotsHostMemoryAllocator:enabled,VulkanVirtualQueue:enabled");
    if let Ok(extra) = std::env::var("EMU_RENDERER_FEATURES") {
        features.push(',');
        features.push_str(&extra);
    }
    let rflags = flags::USE_EGL | flags::USE_VK | flags::VULKAN_NATIVE_SWAPCHAIN | flags::USE_EXTERNAL_BLOB;
    r.init_with_features(w, h, rflags, Some(&features), std::env::var_os("EMU_VERBOSE").is_some(), on_fence)?;
    // Guest-physical to host for the renderer's DMA paths: guest RAM and the address-space area.
    let translate: emu_gfxstream::DmaTranslator = Box::new(move |gpa| {
        let hit = |h: &simee_emu_vmm::vfio_user::AreaHandle| h.get().and_then(|m| m.host_range(gpa, 1));
        hit(&ram).or_else(|| hit(&area)).map_or(std::ptr::null_mut(), |h| h.as_ptr())
    });
    r.set_dma_translator(Some(translate))?;
    let reader = r.clone();
    store.set_readback(Box::new(move |w, h, buf| reader.read_last_posted_to(w, h, buf).is_ok()));
    let chan = simee_emu_vmm::vhost_user::BackendChannel::default();
    let window =
        Arc::new(simee_emu_vmm::vhost_user::ShmemWindow::new(chan.clone(), simee_emu_vmm::virtio::gpu::SHM_HOST_VISIBLE, r.clone()));
    let gpu = Gpu::new(GpuConfig {
        width: w,
        height: h,
        renderer: r.clone(),
        shm: (0, 8 << 30),
        window,
        display: store.clone(),
        verbose: std::env::var_os("EMU_VERBOSE").is_some(),
    });
    Ok((Box::new(gpu), chan, store, r))
}

/// The size of the device's config space: the last offset that reads non-zero, rounded up to 4
/// bytes, as QEMU's generic frontend needs it up front.
#[cfg(target_os = "linux")]
fn config_size(dev: &dyn simee_emu_vmm::virtio::VirtioDevice) -> usize {
    let mut buf = [0u8; 256];
    dev.read_config(0, &mut buf);
    buf.iter().rposition(|&b| b != 0).map_or(0, |i| (i + 1).next_multiple_of(4))
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("soft_devices runs on Linux only");
}
