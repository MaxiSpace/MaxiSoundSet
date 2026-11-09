use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    ptr,
};
use windows::{
    core::{IUnknown, IUnknown_Vtbl, Interface, GUID, HRESULT, PCWSTR},
    Win32::{
        Media::Audio::{
            eMultimedia, eRender, ERole,
            Endpoints::{IAudioEndpointVolume, IAudioMeterInformation},
            IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
            ISimpleAudioVolume, MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED,
            WAVEFORMATEX,
        },
        System::Com::{
            CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
            CLSCTX_ALL, COINIT_APARTMENTTHREADED,
        },
    },
};

pub struct ComScope(bool);
impl ComScope {
    pub fn new() -> Result<Self> {
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if hr.0 == 0x80010106u32 as i32 {
            return Ok(Self(false));
        }
        hr.ok().context("COM initialization failed")?;
        Ok(Self(true))
    }
}
impl Drop for ComScope {
    fn drop(&mut self) {
        if self.0 {
            unsafe {
                CoUninitialize();
            }
        }
    }
}
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
fn enumerator() -> Result<IMMDeviceEnumerator> {
    Ok(unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? })
}
pub fn device(id: &str) -> Result<IMMDevice> {
    let id = wide(id);
    Ok(unsafe { enumerator()?.GetDevice(PCWSTR(id.as_ptr()))? })
}
pub fn default_id(role: i32) -> Result<String> {
    unsafe {
        let d = enumerator()?.GetDefaultAudioEndpoint(eRender, ERole(role))?;
        let p = d.GetId()?;
        let id = p.to_string();
        CoTaskMemFree(Some(p.0.cast()));
        Ok(id?)
    }
}

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub virtual_input: bool,
    pub vb_cable: bool,
    pub verified_physical: bool,
    pub system_default: bool,
}
impl DeviceInfo {
    fn from_endpoint(endpoint: crate::cable_evidence::Endpoint) -> Self {
        let vb_cable = crate::cable_evidence::is_vb_cable_interface(&endpoint.interface);
        let name = endpoint.name.to_ascii_lowercase();
        let verified_physical = endpoint.hardware == crate::cable_evidence::Hardware::Physical && !vb_cable;
        Self { id: endpoint.id, name: endpoint.name, vb_cable, verified_physical, system_default: false,
            virtual_input: vb_cable || name.contains("cable") || name.contains("voicemeeter") || name.contains("virtual audio") }
    }
}
pub fn devices() -> Result<Vec<DeviceInfo>> {
    let _com = ComScope::new()?;
    let inventory = crate::cable_evidence::native_active_hardware_inventory()?;
    anyhow::ensure!(!inventory.enumeration_failed, "Audio endpoint identities could not be completely determined");
    let current = default_id(1).ok();
    Ok(inventory.endpoints.into_iter().filter(|e| e.active).map(DeviceInfo::from_endpoint).map(|mut d| {
        d.system_default = current.as_deref() == Some(d.id.as_str());
        d
    }).collect())
}

pub struct Endpoint {
    pub id: String,
    pub volume: IAudioEndpointVolume,
    pub meter: IAudioMeterInformation,
}
const CONTEXT_GUID: GUID = GUID::from_u128(0x260591ea_56af_4ab1_bcb5_81434160fd04);
impl Endpoint {
    pub fn new(id: &str) -> Result<Self> {
        let d = device(id)?;
        Ok(Self {
            id: id.into(),
            volume: unsafe { d.Activate(CLSCTX_ALL, None)? },
            meter: unsafe { d.Activate(CLSCTX_ALL, None)? },
        })
    }
    pub fn scalar(&self) -> Result<f32> {
        Ok(unsafe { self.volume.GetMasterVolumeLevelScalar()? })
    }
    pub fn set_scalar(&self, v: f32) -> Result<()> {
        unsafe {
            self.volume
                .SetMasterVolumeLevelScalar(v.clamp(0.0, 1.0), &CONTEXT_GUID)?;
        }
        Ok(())
    }
    pub fn db(&self) -> Result<f32> {
        Ok(unsafe { self.volume.GetMasterVolumeLevel()? })
    }
    pub fn set_db(&self, v: f32) -> Result<()> {
        unsafe {
            self.volume.SetMasterVolumeLevel(v, &CONTEXT_GUID)?;
        }
        Ok(())
    }
    pub fn muted(&self) -> Result<bool> {
        Ok(unsafe { self.volume.GetMute()?.as_bool() })
    }
    pub fn peak(&self) -> Result<f32> {
        Ok(unsafe { self.meter.GetPeakValue()? })
    }
    pub fn range(&self) -> Result<(f32, f32, f32)> {
        let (mut min, mut max, mut step) = (0., 0., 0.);
        unsafe {
            self.volume.GetVolumeRange(&mut min, &mut max, &mut step)?;
        }
        Ok((min, max, step))
    }
}

windows::core::define_interface!(
    PolicyConfig,
    PolicyConfigVtbl,
    0xf8679f50_850a_41cf_9c72_430f290290c8
);
#[repr(C)]
pub struct PolicyConfigVtbl {
    pub base__: IUnknown_Vtbl,
    pub reserved: [usize; 10],
    pub set_default_endpoint:
        unsafe extern "system" fn(*mut std::ffi::c_void, PCWSTR, i32) -> HRESULT,
    pub set_visibility: usize,
}
pub fn set_default(id: &str, role: i32) -> Result<()> {
    let class = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
    let unknown: IUnknown = unsafe { CoCreateInstance(&class, None, CLSCTX_ALL)? };
    let policy: PolicyConfig = unknown.cast()?;
    let id = wide(id);
    unsafe {
        (policy.vtable().set_default_endpoint)(policy.as_raw(), PCWSTR(id.as_ptr()), role).ok()?;
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteRestore {
    pub role: i32,
    pub original_id: String,
    pub expected_id: String,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct RecoveryState {
    pub volume_id: String,
    pub original_scalar: f32,
    pub source_id: String,
    pub original_defaults: Vec<String>,
    #[serde(default)]
    pub routes: Vec<RouteRestore>,
}
impl RecoveryState {
    pub fn save(&self, data: &Path) -> Result<()> {
        crate::settings::write_json(&data.join("recovery.json"), &self.compatibility_marker()?)
    }
    fn compatibility_marker(&self) -> Result<Self> {
        let mut marker = self.clone();
        if let Some(first) = self.routes.first() {
            anyhow::ensure!(!first.expected_id.is_empty() && self.routes.iter().all(|route|
                route.expected_id == first.expected_id && !route.original_id.is_empty() && (0..3).contains(&route.role)),
                "Recovery routes need one nonempty owned target and valid roles");
            // Released versions understand one common target. Untouched roles
            // are padded with that target so their legacy restore is a no-op.
            marker.source_id = first.expected_id.clone();
            marker.original_defaults = vec![first.expected_id.clone(); 3];
            for route in &self.routes { marker.original_defaults[route.role as usize] = route.original_id.clone(); }
        }
        Ok(marker)
    }
    fn restore_routes_with(&self, mut read: impl FnMut(i32) -> Result<String>, mut write: impl FnMut(&str, i32) -> Result<()>) -> Result<()> {
        let legacy = self.original_defaults.iter().enumerate().filter(|_| self.routes.is_empty() && !self.source_id.is_empty())
            .map(|(role, id)| RouteRestore { role: role as i32, original_id: id.clone(), expected_id: self.source_id.clone() });
        let mut failures = Vec::new();
        for route in self.routes.iter().cloned().chain(legacy) {
            if let Err(error) = (|| -> Result<()> {
                anyhow::ensure!((0..3).contains(&route.role), "Invalid audio role in recovery");
                if read(route.role)? == route.expected_id { write(&route.original_id, route.role)?; }
                Ok(())
            })() { failures.push(format!("role {}: {error:#}", route.role)); }
        }
        anyhow::ensure!(failures.is_empty(), "{}", failures.join("; "));
        Ok(())
    }
    pub fn restore(&self, data: &Path) -> Result<()> {
        let mut failures = Vec::new();
        if !self.volume_id.is_empty() {
            if let Err(e) =
                Endpoint::new(&self.volume_id).and_then(|ep| ep.set_scalar(self.original_scalar))
            {
                failures.push(format!("Volume restore: {e:#}"));
            }
        }
        if let Err(e) = self.restore_routes_with(default_id, |id, role| set_default(id, role)) {
            failures.push(format!("Route restore: {e:#}"));
        }
        anyhow::ensure!(failures.is_empty(), "{}", failures.join("; "));
        let file = data.join("recovery.json");
        if file.exists() {
            fs::remove_file(file)?;
        }
        Ok(())
    }
}
pub fn recover(data: &Path) -> Result<bool> {
    let file = data.join("recovery.json");
    if !file.exists() {
        return Ok(false);
    }
    let _com = ComScope::new()?;
    let state: RecoveryState = serde_json::from_slice(&fs::read(file)?)?;
    state.restore(data)?;
    log::info!("Previous audio settings recovered");
    Ok(true)
}
pub struct RecoveryGuard {
    pub state: RecoveryState,
    pub data: PathBuf,
    pub restored: bool,
}
impl RecoveryGuard {
    pub fn new(state: RecoveryState, data: &Path) -> Result<Self> {
        state.save(data)?;
        Ok(Self {
            state,
            data: data.into(),
            restored: false,
        })
    }
    /// Save first: a failed commit retains the old durable and in-memory rollback.
    pub fn replace_routes(&mut self, routes: Vec<RouteRestore>) -> Result<()> {
        self.replace_routes_with(routes, |state, data| state.save(data))
    }
    fn replace_routes_with(&mut self, routes: Vec<RouteRestore>, mut save: impl FnMut(&RecoveryState, &Path) -> Result<()>) -> Result<()> {
        let mut next = self.state.clone();
        next.routes = routes;
        next.source_id.clear();
        next.original_defaults.clear();
        save(&next, &self.data)?;
        self.state = next;
        Ok(())
    }
    pub fn route(&mut self, route: RouteRestore) -> Result<bool> {
        self.route_with(route, default_id, |id, role| set_default(id, role), |state, data| state.save(data))
    }
    fn route_with(&mut self, route: RouteRestore,
        mut read: impl FnMut(i32) -> Result<String>,
        mut write: impl FnMut(&str, i32) -> Result<()>,
        mut save: impl FnMut(&RecoveryState, &Path) -> Result<()>) -> Result<bool> {
        if read(route.role)? != route.original_id { return Ok(false); }
        let before = self.state.clone();
        let mut next = before.clone();
        next.routes.push(route.clone());
        save(&next, &self.data)?;
        self.state = next;
        // Durable IO can take time. Recheck immediately before the setter.
        let observed = read(route.role);
        if observed.as_ref().map_or(true, |id| id != &route.original_id) {
            // We know the setter was not attempted. Drop must not restore this
            // role even if the cleanup save itself fails.
            self.state = before;
            save(&self.state, &self.data)?;
            observed?;
            return Ok(false);
        }
        write(&route.expected_id, route.role)?;
        Ok(true)
    }
    pub fn restore(&mut self) -> Result<()> {
        self.state.restore(&self.data)?;
        self.restored = true;
        Ok(())
    }
}
impl Drop for RecoveryGuard {
    fn drop(&mut self) {
        if !self.restored {
            if let Err(e) = self.restore() {
                log::error!("Audio restoration incomplete: {e:#}");
            }
        }
    }
}

pub struct MixFormat {
    raw: *mut WAVEFORMATEX,
    pub rate: usize,
    pub channels: usize,
    pub bits: usize,
    pub float: bool,
    pub mask: u32,
}
impl MixFormat {
    fn new(client: &IAudioClient) -> Result<Self> {
        let raw = unsafe { client.GetMixFormat()? };
        let mut result = Self {
            raw,
            rate: 0,
            channels: 0,
            bits: 0,
            float: false,
            mask: 0,
        };
        unsafe {
            result.rate = (*raw).nSamplesPerSec as usize;
            result.channels = (*raw).nChannels as usize;
            result.bits = (*raw).wBitsPerSample as usize;
            let tag = (*raw).wFormatTag;
            let cb = (*raw).cbSize;
            let p = raw.cast::<u8>();
            if tag == 0xfffe {
                anyhow::ensure!(cb >= 22, "Malformed extensible audio format");
                let sub = ptr::read_unaligned(p.add(24).cast::<GUID>());
                result.mask = ptr::read_unaligned(p.add(20).cast::<u32>());
                result.float = sub == GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
                anyhow::ensure!(
                    result.float || sub == GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71),
                    "Unsupported audio subtype"
                );
            } else {
                result.float = tag == 3;
                anyhow::ensure!(tag == 1 || tag == 3, "Unsupported audio format");
                result.mask = match result.channels {
                    1 => 4,
                    2 => 3,
                    _ => 0,
                };
            }
        }
        anyhow::ensure!(
            (8000..=384000).contains(&result.rate) && (1..=8).contains(&result.channels),
            "Unsupported audio layout"
        );
        anyhow::ensure!(
            (result.float && result.bits == 32)
                || (!result.float && [16, 24, 32].contains(&result.bits)),
            "Unsupported audio bit depth"
        );
        Ok(result)
    }
    fn float_format(&self) -> [u32; 10] {
        let mut output = [0u32; 10];
        let p = output.as_mut_ptr().cast::<u8>();
        unsafe {
            ptr::write_unaligned(p.cast::<u16>(), 0xfffe);
            ptr::write_unaligned(p.add(2).cast::<u16>(), self.channels as u16);
            ptr::write_unaligned(p.add(4).cast::<u32>(), self.rate as u32);
            ptr::write_unaligned(
                p.add(8).cast::<u32>(),
                (self.rate * self.channels * 4) as u32,
            );
            ptr::write_unaligned(p.add(12).cast::<u16>(), (self.channels * 4) as u16);
            ptr::write_unaligned(p.add(14).cast::<u16>(), 32);
            ptr::write_unaligned(p.add(16).cast::<u16>(), 22);
            ptr::write_unaligned(p.add(18).cast::<u16>(), 32);
            ptr::write_unaligned(p.add(20).cast::<u32>(), self.mask);
            ptr::write_unaligned(
                p.add(24).cast::<GUID>(),
                GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71),
            );
        }
        output
    }
    unsafe fn decode(&self, data: *const u8, output: &mut [f32]) {
        for (i, s) in output.iter_mut().enumerate() {
            let p = data.add(i * self.bits / 8);
            *s = if self.float {
                ptr::read_unaligned(p.cast::<f32>())
            } else if self.bits == 16 {
                ptr::read_unaligned(p.cast::<i16>()) as f32 / 32768.0
            } else if self.bits == 24 {
                let n = (*p as i32) | (*p.add(1) as i32) << 8 | (*p.add(2) as i32) << 16;
                ((n << 8) >> 8) as f32 / 8388608.0
            } else {
                (ptr::read_unaligned(p.cast::<i32>()) as f64 / 2147483648.0) as f32
            };
        }
    }
}
impl Drop for MixFormat {
    fn drop(&mut self) {
        unsafe {
            CoTaskMemFree(Some(self.raw.cast()));
        }
    }
}

pub struct CaptureStream {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    pub format: MixFormat,
    started: bool,
}
impl CaptureStream {
    pub fn new(id: &str) -> Result<Self> {
        let client: IAudioClient = unsafe { device(id)?.Activate(CLSCTX_ALL, None)? };
        let format = MixFormat::new(&client)?;
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                0x20000,
                1000000,
                0,
                format.raw,
                None,
            )?;
        }
        let capture: IAudioCaptureClient = unsafe { client.GetService()? };
        Ok(Self {
            client,
            capture,
            format,
            started: false,
        })
    }
    pub fn start(&mut self) -> Result<()> {
        unsafe {
            self.client.Start()?;
        }
        self.started = true;
        Ok(())
    }
    pub fn next_packet(&self) -> Result<usize> {
        Ok(unsafe { self.capture.GetNextPacketSize()? } as usize)
    }
    pub fn read(&self, output: &mut Vec<f32>) -> Result<usize> {
        let (mut data, mut frames, mut flags) = (ptr::null_mut(), 0, 0);
        unsafe {
            self.capture
                .GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
        }
        struct Packet<'a>(&'a IAudioCaptureClient, u32);
        impl Drop for Packet<'_> {
            fn drop(&mut self) {
                unsafe {
                    let _ = self.0.ReleaseBuffer(self.1);
                }
            }
        }
        let _release = Packet(&self.capture, frames);
        output.resize(frames as usize * self.format.channels, 0.0);
        if flags & 2 != 0 {
            output.fill(0.0);
        } else if frames > 0 {
            anyhow::ensure!(!data.is_null(), "Missing audio packet data");
            unsafe {
                self.format.decode(data, output);
            }
        }
        Ok(frames as usize)
    }
}
impl Drop for CaptureStream {
    fn drop(&mut self) {
        if self.started {
            unsafe {
                let _ = self.client.Stop();
            }
        }
    }
}

pub struct RenderStream {
    client: IAudioClient,
    render: IAudioRenderClient,
    pub frames: usize,
    channels: usize,
    started: bool,
}
impl RenderStream {
    pub fn new(id: &str, format: &MixFormat) -> Result<Self> {
        let client: IAudioClient = unsafe { device(id)?.Activate(CLSCTX_ALL, None)? };
        let raw = format.float_format();
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                0x88080000,
                500000,
                0,
                raw.as_ptr().cast(),
                Some(&GUID::from_u128(0xa828fc36_90c6_4baa_b58d_83546c8a7321)),
            )?;
        }
        // Dedicated non-persistent session avoids a second volume-mixer attenuation.
        let session: ISimpleAudioVolume = unsafe { client.GetService()? };
        unsafe {
            session.SetMasterVolume(1.0, &CONTEXT_GUID)?;
            session.SetMute(false, &CONTEXT_GUID)?;
        }
        let render = unsafe { client.GetService()? };
        let frames = unsafe { client.GetBufferSize()? } as usize;
        Ok(Self {
            client,
            render,
            frames,
            channels: format.channels,
            started: false,
        })
    }
    pub fn start(&mut self) -> Result<()> {
        unsafe {
            let _p = self.render.GetBuffer(self.frames as u32)?;
            self.render.ReleaseBuffer(self.frames as u32, 2)?;
            self.client.Start()?;
        }
        self.started = true;
        Ok(())
    }
    pub fn available(&self) -> Result<usize> {
        Ok(self
            .frames
            .saturating_sub(unsafe { self.client.GetCurrentPadding()? } as usize))
    }
    pub fn write(&self, samples: &[f32]) -> Result<()> {
        anyhow::ensure!(
            samples.len() % self.channels == 0,
            "Unaligned render packet"
        );
        let frames = (samples.len() / self.channels) as u32;
        if frames == 0 {
            return Ok(());
        }
        unsafe {
            let p = self.render.GetBuffer(frames)?;
            ptr::copy_nonoverlapping(samples.as_ptr().cast::<u8>(), p, samples.len() * 4);
            self.render.ReleaseBuffer(frames, 0)?;
        }
        Ok(())
    }
}
impl Drop for RenderStream {
    fn drop(&mut self) {
        if self.started {
            unsafe {
                let _ = self.client.Stop();
            }
        }
    }
}

pub fn diagnose() -> Result<String> {
    let _com = ComScope::new()?;
    let mut report = format!(
        "Maxi Sound Set {} / Rust - read-only audio diagnostics\n",
        env!("CARGO_PKG_VERSION")
    );
    for d in devices()? {
        report += &format!(
            "{} | {} | {}\n",
            if d.virtual_input { "VIRTUAL" } else { "OUTPUT" },
            d.name,
            d.id
        );
    }
    let id = default_id(eMultimedia.0)?;
    let ep = Endpoint::new(&id)?;
    let original = ep.scalar()?;
    report += &format!(
        "Default: {id}\nVolume: {original}, {} dB, mute={}\n",
        ep.db()?,
        ep.muted()?
    );
    let mut capture = CaptureStream::new(&id)?;
    capture.start()?;
    let renderer = RenderStream::new(&id, &capture.format)?;
    report += &format!("PASS WASAPI loopback initialized and started: {} Hz, {} channels.\nPASS float renderer initialized: {} frames. No audio emitted.\n", capture.format.rate, capture.format.channels, renderer.frames);
    anyhow::ensure!(
        (ep.scalar()? - original).abs() < 0.001,
        "Diagnostic changed volume"
    );
    Ok(report)
}

#[cfg(test)]
mod cable_identity_tests {
    use super::*;
    #[test]
    fn cable_identity_survives_rename_and_id_change_without_claiming_other_virtual_devices() {
        let endpoint = |id: &str, name: &str, interface: &str| crate::cable_evidence::Endpoint {
            id: id.into(), name: name.into(), interface: interface.into(), active: true, hardware: crate::cable_evidence::Hardware::Unknown,
        };
        for (id, name) in [("old-id", "ورودی صدا"), ("new-id", "CABLE In 16ch"), ("new-id", "Renamed input")] {
            let device = DeviceInfo::from_endpoint(endpoint(id, name, "VB-Audio Virtual Cable"));
            assert!(device.vb_cable && device.virtual_input);
            assert_eq!(device.id, id);
        }
        let other = DeviceInfo::from_endpoint(endpoint("other", "Voicemeeter Input", "VB-Audio Voicemeeter"));
        assert!(other.virtual_input && !other.vb_cable);
        let physical = DeviceInfo::from_endpoint(endpoint("physical", "Speakers", "USB Audio"));
        assert!(!physical.virtual_input && !physical.vb_cable);
    }
    #[test]
    fn legacy_software_outputs_preserve_manual_intent_without_becoming_physical_fallbacks() {
        use crate::{audio_routing, cable_evidence::{Endpoint, Hardware}, settings::Settings};
        for hardware in [Hardware::Software, Hardware::Unknown] {
            let cable = DeviceInfo::from_endpoint(Endpoint {
                id: "cable".into(), name: "Renamed input".into(), interface: "VB-Audio Virtual Cable".into(), active: true, hardware,
            });
            assert!(cable.virtual_input && cable.vb_cable);
            assert!(!cable.verified_physical);
            for name in ["FxSound Speakers", "7.1 Surround Sound", "Steam Streaming Speakers"] {
                let output = DeviceInfo::from_endpoint(Endpoint {
                    id: "saved-output".into(), name: name.into(), interface: name.into(), active: true, hardware,
                });
                assert!(!output.virtual_input && !output.vb_cable, "Legacy playback output: {name}");
                assert!(!output.verified_physical, "Software/unknown hardware must not be learned as physical");
                let devices = vec![cable.clone(), output];
                let mut settings = Settings {
                    source_id: "cable".into(), output_id: "saved-output".into(),
                    last_physical_output_id: "saved-output".into(), auto_route: false, mode: 1,
                    ..Settings::default()
                };
                let resolved = audio_routing::resolve_output(&settings, &devices, Some("external-forwarder")).unwrap();
                assert_eq!(resolved, "saved-output");
                let defaults = vec!["external-forwarder".into(); 3];
                assert!(audio_routing::windows_routes(&settings, &devices, &defaults, &resolved).unwrap().is_empty());
                assert!(audio_routing::full_routes(&settings, &defaults).is_empty());
                settings.output_id.clear();
                settings.auto_route = true;
                assert!(audio_routing::resolve_output(&settings, &devices, Some("cable")).is_err(),
                    "Remembering a legacy output ID must not turn it into a physical fallback");
            }
        }
    }
}

#[cfg(test)]
mod routing_recovery_tests {
    use super::*;
    use std::{cell::{Cell, RefCell}, rc::Rc};

    fn route(role: i32) -> RouteRestore {
        RouteRestore { role, original_id: "cable".into(), expected_id: "speaker".into() }
    }
    fn guard() -> RecoveryGuard {
        // This injected guard never calls native audio. Drop is disabled because
        // tests explicitly exercise recovery with an in-memory role store.
        RecoveryGuard { state: RecoveryState::default(), data: PathBuf::new(), restored: true }
    }
    #[test]
    fn partial_role_failure_keeps_durable_originals_and_ownership_for_recovery() {
        let roles = Rc::new(RefCell::new(vec!["cable".to_owned(); 3]));
        let durable = RefCell::new(RecoveryState::default());
        let mut guard = guard();
        for role in 0..2 {
            let result = guard.route_with(route(role), |r| Ok(roles.borrow()[r as usize].clone()), |id, r| {
                assert!(durable.borrow().routes.iter().any(|v| v.role == r), "Must be durable before the setter");
                if r == 1 { anyhow::bail!("Injected second-role failure"); }
                roles.borrow_mut()[r as usize] = id.into();
                Ok(())
            }, |state, _| { *durable.borrow_mut() = state.clone(); Ok(()) });
            assert_eq!(result.is_ok(), role == 0);
        }
        assert_eq!(&*roles.borrow(), &["speaker", "cable", "cable"]);
        assert_eq!(durable.borrow().routes.len(), 2);
        // A user changes a role we touched. Recovery must leave it alone.
        roles.borrow_mut()[1] = "external-headphones".into();
        durable.borrow().restore_routes_with(|r| Ok(roles.borrow()[r as usize].clone()), |id, r| {
            roles.borrow_mut()[r as usize] = id.into(); Ok(())
        }).unwrap();
        assert_eq!(&*roles.borrow(), &["cable", "external-headphones", "cable"]);
    }
    #[test]
    fn pre_mutation_save_failure_and_changed_snapshot_never_call_setter() {
        let writes = Cell::new(0);
        let mut guard = guard();
        let result = guard.route_with(route(0), |_| Ok("cable".into()), |_, _| { writes.set(writes.get() + 1); Ok(()) }, |_, _| anyhow::bail!("Injected write failure"));
        assert!(result.is_err());
        assert_eq!(writes.get(), 0);
        assert!(guard.state.routes.is_empty());
        let mut reads = 0;
        let durable = RefCell::new(RecoveryState::default());
        assert!(!guard.route_with(route(0), |_| {
            reads += 1; Ok(if reads == 1 { "cable" } else { "external" }.into())
        }, |_, _| { writes.set(writes.get() + 1); Ok(()) }, |state, _| { *durable.borrow_mut() = state.clone(); Ok(()) }).unwrap());
        assert!(guard.state.routes.is_empty() && durable.borrow().routes.is_empty());
        assert_eq!(writes.get(), 0);
    }
    #[test]
    fn skipped_role_cleanup_failure_cannot_create_in_memory_ownership() {
        let mut guard = guard();
        let mut reads = 0;
        let mut saves = 0;
        assert!(guard.route_with(route(0), |_| {
            reads += 1; Ok(if reads == 1 { "cable" } else { "external" }.into())
        }, |_, _| panic!("Skipped role must not be set"), |_, _| {
            saves += 1; anyhow::ensure!(saves == 1, "Injected cleanup failure"); Ok(())
        }).is_err());
        assert!(guard.state.routes.is_empty());
    }
    #[test]
    fn commit_save_failure_retains_rollback_but_success_releases_normalized_defaults() {
        let mut guard = guard();
        guard.state.routes.push(route(1));
        let durable = RefCell::new(guard.state.clone());
        assert!(guard.replace_routes_with(Vec::new(), |_, _| anyhow::bail!("Injected commit failure")).is_err());
        assert_eq!(guard.state.routes, durable.borrow().routes);
        let roles = RefCell::new(vec!["cable".to_owned(), "speaker".to_owned(), "cable".to_owned()]);
        durable.borrow().restore_routes_with(|r| Ok(roles.borrow()[r as usize].clone()), |id, r| {
            roles.borrow_mut()[r as usize] = id.into(); Ok(())
        }).unwrap();
        assert_eq!(roles.borrow()[1], "cable", "Failed commit must still restore the actual startup baseline");
        guard.replace_routes_with(Vec::new(), |state, _| { *durable.borrow_mut() = state.clone(); Ok(()) }).unwrap();
        assert!(guard.state.routes.is_empty() && durable.borrow().routes.is_empty());
        durable.borrow().restore_routes_with(|_| panic!("Committed normalization has no route rollback"), |_, _| panic!("No route write")).unwrap();
    }
    #[test]
    fn failed_commit_preserves_real_durable_recovery_marker() {
        let directory = std::env::temp_dir().join(format!("maxi-route-marker-{}-{:?}", std::process::id(), std::thread::current().id()));
        let state = RecoveryState { routes: vec![route(1)], ..Default::default() };
        let mut guard = RecoveryGuard::new(state, &directory).unwrap();
        guard.restored = true; // Injected recovery only; never touch native audio.
        let before = fs::read(directory.join("recovery.json")).unwrap();
        assert!(guard.replace_routes_with(Vec::new(), |_, _| anyhow::bail!("Injected commit save failure")).is_err());
        assert_eq!(fs::read(directory.join("recovery.json")).unwrap(), before);
        let loaded: RecoveryState = serde_json::from_slice(&before).unwrap();
        let restored = RefCell::new(Vec::new());
        loaded.restore_routes_with(|_| Ok("speaker".into()), |id, role| { restored.borrow_mut().push((role, id.to_owned())); Ok(()) }).unwrap();
        assert_eq!(&*restored.borrow(), &[(1, "cable".into())]);
        fs::remove_file(directory.join("recovery.json")).unwrap();
        fs::remove_dir(directory).unwrap();
    }
    #[test]
    fn legacy_recovery_marker_keeps_role_specific_ownership_and_settings_migrate() {
        let state: RecoveryState = serde_json::from_str(r#"{"volume_id":"","original_scalar":0.5,"source_id":"cable","original_defaults":["console","multimedia","communications"]}"#).unwrap();
        assert!(state.routes.is_empty());
        let restored = RefCell::new(Vec::new());
        state.restore_routes_with(|role| Ok(if role == 1 { "external" } else { "cable" }.into()), |id, role| { restored.borrow_mut().push((role, id.to_owned())); Ok(()) }).unwrap();
        assert_eq!(&*restored.borrow(), &[(0, "console".into()), (2, "communications".into())]);
        let old: crate::settings::Settings = serde_json::from_str("{}").unwrap();
        assert!(old.last_physical_output_id.is_empty());
    }
    #[test]
    fn new_marker_is_readable_by_released_recovery_and_committed_windows_clears_mirrors() {
        #[derive(Deserialize)]
        struct ReleasedRecovery { source_id: String, original_defaults: Vec<String> }
        let mut guard = guard();
        let initial = guard.state.compatibility_marker().unwrap();
        assert!(initial.source_id.is_empty() && initial.original_defaults.is_empty());
        guard.state.routes.push(route(1));
        let marker = guard.state.compatibility_marker().unwrap();
        let bytes = serde_json::to_vec(&marker).unwrap();
        let released: ReleasedRecovery = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(released.source_id, "speaker");
        assert_eq!(released.original_defaults, ["speaker", "cable", "speaker"]);
        let mut defaults = vec!["speaker".to_owned(), "speaker".to_owned(), "external".to_owned()];
        let mut changed = Vec::new();
        for (role, original) in released.original_defaults.iter().enumerate() {
            if defaults[role] == released.source_id {
                if defaults[role] != *original { changed.push(role); }
                defaults[role] = original.clone();
            }
        }
        assert_eq!(defaults, ["speaker", "cable", "external"]);
        assert_eq!(changed, [1], "Released recovery changes only the attempted role");
        let new_reader: RecoveryState = serde_json::from_slice(&bytes).unwrap();
        let writes = Cell::new(0);
        new_reader.restore_routes_with(|_| Ok("speaker".into()), |_, _| { writes.set(writes.get() + 1); Ok(()) }).unwrap();
        assert_eq!(writes.get(), 1, "New reader must not replay legacy mirrors too");
        guard.replace_routes_with(Vec::new(), |state, _| {
            let committed = state.compatibility_marker()?;
            assert!(committed.routes.is_empty() && committed.source_id.is_empty() && committed.original_defaults.is_empty());
            Ok(())
        }).unwrap();
    }
}
