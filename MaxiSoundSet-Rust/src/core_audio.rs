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
        Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
        Media::Audio::{
            eMultimedia, eRender, ERole,
            Endpoints::{IAudioEndpointVolume, IAudioMeterInformation},
            IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
            ISimpleAudioVolume, MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, DEVICE_STATE_ACTIVE,
            WAVEFORMATEX,
        },
        System::Com::{
            CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
            StructuredStorage::{PropVariantClear, PropVariantToStringAlloc},
            CLSCTX_ALL, COINIT_APARTMENTTHREADED, STGM_READ,
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
}
pub fn devices() -> Result<Vec<DeviceInfo>> {
    let _com = ComScope::new()?;
    let mut result = Vec::new();
    unsafe {
        let collection = enumerator()?.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        for i in 0..collection.GetCount()? {
            let d = collection.Item(i)?;
            let p = d.GetId()?;
            let id = p.to_string();
            CoTaskMemFree(Some(p.0.cast()));
            let id = id?;
            let store = d.OpenPropertyStore(STGM_READ)?;
            let mut value = store.GetValue(&PKEY_Device_FriendlyName)?;
            let name = match PropVariantToStringAlloc(&value) {
                Ok(p) => {
                    let s = p.to_string().unwrap_or_else(|_| id.clone());
                    CoTaskMemFree(Some(p.0.cast()));
                    s
                }
                Err(_) => id.clone(),
            };
            let _ = PropVariantClear(&mut value);
            let n = name.to_ascii_lowercase();
            let virtual_input =
                n.contains("cable") || n.contains("voicemeeter") || n.contains("virtual audio");
            result.push(DeviceInfo {
                id,
                name,
                virtual_input,
            });
        }
    }
    Ok(result)
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

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct RecoveryState {
    pub volume_id: String,
    pub original_scalar: f32,
    pub source_id: String,
    pub original_defaults: Vec<String>,
}
impl RecoveryState {
    pub fn save(&self, data: &Path) -> Result<()> {
        crate::settings::write_json(&data.join("recovery.json"), self)
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
        if !self.source_id.is_empty() {
            for (role, original) in self.original_defaults.iter().enumerate() {
                if let Err(e) = (|| -> Result<()> {
                    if default_id(role as i32)? == self.source_id {
                        set_default(original, role as i32)?;
                    }
                    Ok(())
                })() {
                    failures.push(format!("Route restore: {e:#}"));
                }
            }
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
