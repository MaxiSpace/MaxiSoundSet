//! Read-only endpoint observations and conservative forward installation evidence.
//! The record is evidence of one observed attempt, never exclusive driver ownership.
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

pub const RECORD_FILE: &str = "vb-cable-provenance.json";
pub fn is_vb_cable_interface(name: &str) -> bool {
    name.trim().eq_ignore_ascii_case("VB-Audio Virtual Cable")
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "presence", content = "endpoint_ids")]
pub enum Observation {
    Present(Vec<String>),
    Absent,
    Unknown,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallRecord {
    pub schema_version: u32,
    pub recorded_unix: u64,
    pub before: Observation,
    pub signature_verified: bool,
    pub package_sha256: String,
    pub official_exit: Option<u32>,
    pub after: Observation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_app_attempt: Option<Box<InstallRecord>>,
}
impl InstallRecord {
    pub fn pending(before: Observation, package_sha256: String) -> Self {
        Self {
            schema_version: 1,
            recorded_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
            before,
            signature_verified: true,
            package_sha256,
            official_exit: None,
            after: Observation::Unknown,
            initial_app_attempt: None,
        }
    }
    pub fn pending_with_prior(
        before: Observation,
        package_sha256: String,
        prior: Option<&Self>,
    ) -> Self {
        let preserve =
            prior.is_some_and(|p| provenance(Some(p), &p.after) == Provenance::AppInstallObserved);
        let mut record = Self::pending(before, package_sha256);
        if preserve {
            if let Some(prior) = prior {
                record.initial_app_attempt = prior
                    .initial_app_attempt
                    .clone()
                    .or_else(|| Some(Box::new(prior.clone())));
            }
        }
        record
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provenance {
    Preexisting,
    AppInstallObserved,
    Unknown,
}
fn valid_ids(ids: &[String]) -> bool {
    !ids.is_empty()
        && ids.iter().all(|id| !id.trim().is_empty())
        && ids.iter().collect::<std::collections::HashSet<_>>().len() == ids.len()
}
fn same_ids(a: &[String], b: &[String]) -> bool {
    valid_ids(a) && valid_ids(b) && a.len() == b.len() && a.iter().all(|id| b.contains(id))
}
pub fn provenance(record: Option<&InstallRecord>, current: &Observation) -> Provenance {
    let Some(record) = record else {
        return Provenance::Unknown;
    };
    if record.schema_version != 1
        || record.recorded_unix == 0
        || !record.signature_verified
        || !matches!(record.official_exit, Some(0 | 3010))
        || record.package_sha256.len() != 64
        || !record.package_sha256.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Provenance::Unknown;
    }
    let (Observation::Present(after), Observation::Present(now)) = (&record.after, current) else {
        return Provenance::Unknown;
    };
    if !same_ids(after, now) {
        return Provenance::Unknown;
    }
    match &record.before {
        Observation::Absent => Provenance::AppInstallObserved,
        Observation::Present(ids) if valid_ids(ids) => {
            if let Some(initial) = record.initial_app_attempt.as_deref() {
                if same_ids(ids, after)
                    && matches!(initial.before, Observation::Absent)
                    && provenance(Some(initial), &record.before) == Provenance::AppInstallObserved
                {
                    Provenance::AppInstallObserved
                } else {
                    Provenance::Unknown
                }
            } else {
                Provenance::Preexisting
            }
        }
        _ => Provenance::Unknown,
    }
}
pub fn read_record(data: &Path) -> Option<InstallRecord> {
    let path = data.join(RECORD_FILE);
    let metadata = fs::symlink_metadata(&path).ok()?;
    if reparse_or_symlink(&metadata) || !metadata.is_file() || metadata.len() > 64 * 1024 {
        return None;
    }
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hardware {
    Physical,
    Software,
    Unknown,
}
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub id: String,
    pub name: String,
    pub interface: String,
    pub active: bool,
    pub hardware: Hardware,
}
#[derive(Clone, Debug, Default)]
pub struct Inventory {
    pub endpoints: Vec<Endpoint>,
    pub enumeration_failed: bool,
}
impl Inventory {
    pub fn strict_observation(&self) -> Observation {
        if self.enumeration_failed {
            Observation::Unknown
        } else {
            self.observation()
        }
    }
    pub fn observation(&self) -> Observation {
        let mut ids: Vec<String> = self
            .endpoints
            .iter()
            .filter(|e| is_vb_cable_interface(&e.interface))
            .map(|e| e.id.clone())
            .collect();
        ids.sort();
        ids.dedup();
        if !ids.is_empty() {
            Observation::Present(ids)
        } else if self.enumeration_failed {
            Observation::Unknown
        } else {
            Observation::Absent
        }
    }
}
pub fn hardware_enumerator(name: &str) -> Hardware {
    match name.trim().to_ascii_uppercase().as_str() {
        "USB" | "HDAUDIO" | "BTHENUM" | "BTHHFENUM" => Hardware::Physical,
        "ROOT" | "SWD" | "SW" => Hardware::Software,
        _ => Hardware::Unknown,
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fallback {
    Candidate { id: String, name: String },
    Unavailable,
    Unknown,
    ConfiguredInvalid,
}
pub fn fallback(inventory: &Inventory, configured: Result<Option<String>>) -> Fallback {
    let Ok(configured) = configured else {
        return Fallback::Unknown;
    };
    let eligible = |e: &&Endpoint| {
        e.active && e.hardware == Hardware::Physical && !is_vb_cable_interface(&e.interface)
    };
    if let Some(id) = configured.filter(|id| !id.is_empty()) {
        match inventory.endpoints.iter().find(|e| e.id == id) {
            Some(e) if eligible(&e) => {
                return Fallback::Candidate {
                    id: e.id.clone(),
                    name: e.name.clone(),
                }
            }
            Some(e) if e.hardware == Hardware::Unknown => return Fallback::Unknown,
            _ if inventory.enumeration_failed => return Fallback::Unknown,
            _ => return Fallback::ConfiguredInvalid,
        }
    }
    if let Some(e) = inventory.endpoints.iter().find(eligible) {
        return Fallback::Candidate {
            id: e.id.clone(),
            name: e.name.clone(),
        };
    }
    if inventory.enumeration_failed
        || inventory.endpoints.iter().any(|e| {
            e.active && e.hardware == Hardware::Unknown && !is_vb_cable_interface(&e.interface)
        })
    {
        Fallback::Unknown
    } else {
        Fallback::Unavailable
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Defaults {
    Physical,
    CableRoute,
    Invalid,
    Unknown,
}
pub fn defaults(inventory: &Inventory, ids: Result<Vec<String>>) -> Defaults {
    let Ok(ids) = ids else {
        return Defaults::Unknown;
    };
    if ids.len() != 3 || ids.iter().any(String::is_empty) {
        return Defaults::Invalid;
    }
    let mut unknown = false;
    let mut invalid = false;
    let mut cable = false;
    for id in ids {
        match inventory.endpoints.iter().find(|e| e.id == id) {
            Some(e) if is_vb_cable_interface(&e.interface) => cable = true,
            Some(e) if e.active && e.hardware == Hardware::Physical => (),
            Some(e) if e.hardware == Hardware::Unknown => unknown = true,
            None if inventory.enumeration_failed => unknown = true,
            _ => invalid = true,
        }
    }
    if cable {
        Defaults::CableRoute
    } else if unknown {
        Defaults::Unknown
    } else if invalid {
        Defaults::Invalid
    } else {
        Defaults::Physical
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recovery {
    Clear,
    Pending,
    Invalid,
    Unknown,
}
fn reparse_or_symlink(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    metadata.file_type().is_symlink()
}
pub fn recovery(data: &Path) -> Recovery {
    let path = data.join("recovery.json");
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Recovery::Clear,
        Err(_) => return Recovery::Unknown,
        Ok(m) if reparse_or_symlink(&m) || !m.is_file() || m.len() > 64 * 1024 => {
            return Recovery::Invalid
        }
        Ok(_) => (),
    }
    recovery_contents(fs::read(path).map(Some).map_err(Into::into))
}
pub fn recovery_contents(contents: Result<Option<Vec<u8>>>) -> Recovery {
    let bytes = match contents {
        Ok(None) => return Recovery::Clear,
        Ok(Some(bytes)) => bytes,
        Err(_) => return Recovery::Unknown,
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Recovery::Invalid;
    };
    if value["volume_id"].is_string()
        && value["source_id"].is_string()
        && value["original_scalar"]
            .as_f64()
            .is_some_and(|n| (0.0..=1.0).contains(&n))
        && value["original_defaults"]
            .as_array()
            .is_some_and(|ids| ids.len() <= 3 && ids.iter().all(serde_json::Value::is_string))
    {
        Recovery::Pending
    } else {
        Recovery::Invalid
    }
}
pub fn configured_output(data: &Path) -> Result<Option<String>> {
    let path = data.join("settings.json");
    let bytes = match fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        result => result?,
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    ensure!(value.is_object(), "Settings must be an object");
    match value.get("output_id") {
        None => Ok(None),
        Some(serde_json::Value::String(id)) => Ok(Some(id.clone())),
        _ => anyhow::bail!("Invalid configured output identity"),
    }
}

struct Com(bool);
impl Com {
    fn new() -> Result<Self> {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if hr.0 == 0x80010106u32 as i32 {
            return Ok(Self(false));
        }
        hr.ok()?;
        Ok(Self(true))
    }
}
impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            unsafe {
                windows::Win32::System::Com::CoUninitialize();
            }
        }
    }
}
fn allocated_text(value: windows::core::PWSTR) -> Result<String> {
    let text = unsafe { value.to_string() };
    unsafe {
        windows::Win32::System::Com::CoTaskMemFree(Some(value.0.cast()));
    }
    let text = text?;
    ensure!(!text.trim().is_empty(), "Empty endpoint identity");
    Ok(text)
}
fn property(
    device: &windows::Win32::Media::Audio::IMMDevice,
    key: &windows::Win32::Foundation::PROPERTYKEY,
) -> Result<String> {
    use windows::Win32::System::Com::{
        StructuredStorage::{PropVariantClear, PropVariantToStringAlloc},
        STGM_READ,
    };
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ)?;
        let mut value = store.GetValue(key)?;
        let result = PropVariantToStringAlloc(&value);
        let clear = PropVariantClear(&mut value);
        let text = allocated_text(result?)?;
        clear?;
        Ok(text)
    }
}
fn pnp_enumerator_name(instance_id: &str) -> Result<String> {
    use windows::{
        core::PCWSTR,
        Win32::Devices::{
            DeviceAndDriverInstallation::{
                CM_Get_DevNode_PropertyW, CM_Locate_DevNodeW, CM_LOCATE_DEVNODE_NORMAL,
                CR_BUFFER_SMALL, CR_SUCCESS,
            },
            Properties::{DEVPKEY_Device_EnumeratorName, DEVPROPTYPE, DEVPROP_TYPE_STRING},
        },
    };
    let wide: Vec<u16> = instance_id.encode_utf16().chain(Some(0)).collect();
    let mut node = 0;
    unsafe {
        let status = CM_Locate_DevNodeW(&mut node, PCWSTR(wide.as_ptr()), CM_LOCATE_DEVNODE_NORMAL);
        ensure!(
            status == CR_SUCCESS,
            "PnP locate failed: CONFIGRET {}",
            status.0
        );
        let mut kind = DEVPROPTYPE(0);
        let mut size = 0;
        let status = CM_Get_DevNode_PropertyW(
            node,
            &DEVPKEY_Device_EnumeratorName,
            &mut kind,
            None,
            &mut size,
            0,
        );
        ensure!(
            status == CR_BUFFER_SMALL || status == CR_SUCCESS,
            "PnP enumerator size read failed: CONFIGRET {}",
            status.0
        );
        ensure!(
            (2..=65536).contains(&size) && size % 2 == 0,
            "Invalid PnP enumerator property size"
        );
        let mut bytes = vec![0u8; size as usize];
        let status = CM_Get_DevNode_PropertyW(
            node,
            &DEVPKEY_Device_EnumeratorName,
            &mut kind,
            Some(bytes.as_mut_ptr()),
            &mut size,
            0,
        );
        ensure!(
            status == CR_SUCCESS
                && kind == DEVPROP_TYPE_STRING
                && size as usize <= bytes.len()
                && size % 2 == 0,
            "PnP enumerator read/type failed: CONFIGRET {}",
            status.0
        );
        let units: Vec<u16> = bytes[..size as usize]
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        let name = String::from_utf16(&units)?
            .trim_end_matches('\0')
            .to_owned();
        ensure!(!name.trim().is_empty(), "Empty PnP enumerator identity");
        Ok(name)
    }
}
fn adapter_enumerator(adapter: &windows::Win32::Media::Audio::IMMDevice) -> Result<String> {
    use windows::Win32::Devices::FunctionDiscovery::{
        PKEY_Device_EnumeratorName, PKEY_Device_InstanceId,
    };
    // CoreAudio need not expose this PnP property. Its exact instance ID is the
    // documented bridge to the read-only ConfigMgr property query, never parsed.
    property(adapter, &PKEY_Device_EnumeratorName)
        .or_else(|_| pnp_enumerator_name(&property(adapter, &PKEY_Device_InstanceId)?))
}
pub fn native_inventory(check_hardware: bool) -> Result<Inventory> {
    use windows::Win32::Media::Audio::{DEVICE_STATE, DEVICE_STATEMASK_ALL};
    native_inventory_for(check_hardware, DEVICE_STATE(DEVICE_STATEMASK_ALL))
}
pub fn native_active_inventory() -> Result<Inventory> {
    native_inventory_for(false, windows::Win32::Media::Audio::DEVICE_STATE_ACTIVE)
}
fn native_inventory_for(check_hardware: bool, states: windows::Win32::Media::Audio::DEVICE_STATE) -> Result<Inventory> {
    use windows::{
        core::PCWSTR,
        Win32::{
            Devices::FunctionDiscovery::{
                PKEY_DeviceInterface_FriendlyName, PKEY_Device_FriendlyName,
            },
            Media::Audio::{
                eRender, IDeviceTopology, IMMDeviceEnumerator, MMDeviceEnumerator,
                DEVICE_STATE_ACTIVE,
            },
            System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER},
        },
    };
    let _com = Com::new()?;
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER)?;
        let devices = enumerator.EnumAudioEndpoints(eRender, states)?;
        let mut inventory = Inventory::default();
        for index in 0..devices.GetCount()? {
            let result = (|| -> Result<Endpoint> {
                let device = devices.Item(index)?;
                let id = allocated_text(device.GetId()?)?;
                let interface = property(&device, &PKEY_DeviceInterface_FriendlyName)?;
                let active = device.GetState()? == DEVICE_STATE_ACTIVE;
                let name =
                    property(&device, &PKEY_Device_FriendlyName).unwrap_or_else(|_| id.clone());
                let hardware = if check_hardware && active && !is_vb_cable_interface(&interface) {
                    (|| -> Result<Hardware> {
                        let topology: IDeviceTopology =
                            device.Activate(CLSCTX_INPROC_SERVER, None)?;
                        let adapter_id =
                            allocated_text(topology.GetConnector(0)?.GetDeviceIdConnectedTo()?)?;
                        let wide: Vec<u16> = adapter_id.encode_utf16().chain(Some(0)).collect();
                        let adapter = enumerator.GetDevice(PCWSTR(wide.as_ptr()))?;
                        Ok(hardware_enumerator(&adapter_enumerator(&adapter)?))
                    })()
                    .unwrap_or(Hardware::Unknown)
                } else {
                    Hardware::Unknown
                };
                Ok(Endpoint {
                    id,
                    name,
                    interface,
                    active,
                    hardware,
                })
            })();
            match result {
                Ok(endpoint) => inventory.endpoints.push(endpoint),
                Err(_) => inventory.enumeration_failed = true,
            }
        }
        Ok(inventory)
    }
}
pub fn observe_native() -> Observation {
    native_inventory(false)
        .map(|i| i.strict_observation())
        .unwrap_or(Observation::Unknown)
}
pub fn native_defaults() -> Result<Vec<String>> {
    use windows::Win32::{
        Media::Audio::{eRender, ERole, IMMDeviceEnumerator, MMDeviceEnumerator},
        System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER},
    };
    let _com = Com::new()?;
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER)? };
    let reads: Vec<Result<String>> = (0..3)
        .map(|role| unsafe {
            allocated_text(
                enumerator
                    .GetDefaultAudioEndpoint(eRender, ERole(role))?
                    .GetId()?,
            )
        })
        .collect();
    reads.into_iter().collect()
}

pub fn native_adapter_diagnostics() -> Result<Vec<serde_json::Value>> {
    use windows::{
        core::PCWSTR,
        Win32::{
            Devices::FunctionDiscovery::{PKEY_Device_EnumeratorName, PKEY_Device_InstanceId},
            Media::Audio::{
                eRender, IDeviceTopology, IMMDeviceEnumerator, MMDeviceEnumerator,
                DEVICE_STATE_ACTIVE,
            },
            System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER},
        },
    };
    let _com = Com::new()?;
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER)?;
        let collection = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        let mut results = Vec::new();
        for index in 0..collection.GetCount()? {
            let result = (|| -> Result<serde_json::Value> {
                let endpoint = collection.Item(index)?;
                let endpoint_id = allocated_text(endpoint.GetId()?)?;
                let topology: IDeviceTopology = endpoint.Activate(CLSCTX_INPROC_SERVER, None)?;
                let adapter_id =
                    allocated_text(topology.GetConnector(0)?.GetDeviceIdConnectedTo()?)?;
                let wide: Vec<u16> = adapter_id.encode_utf16().chain(Some(0)).collect();
                let adapter = enumerator.GetDevice(PCWSTR(wide.as_ptr()))?;
                let instance =
                    property(&adapter, &PKEY_Device_InstanceId).map_err(|e| format!("{e:#}"));
                let name =
                    property(&adapter, &PKEY_Device_EnumeratorName).map_err(|e| format!("{e:#}"));
                let resolved = adapter_enumerator(&adapter).map_err(|e| format!("{e:#}"));
                Ok(
                    serde_json::json!({"endpoint_id":endpoint_id,"adapter_id":adapter_id,"device_instance_id":instance,"property_enumerator_name":name,"resolved_enumerator_name":resolved}),
                )
            })();
            results.push(match result {Ok(value)=>value,Err(e)=>serde_json::json!({"endpoint_index":index,"adapter_lookup_error":format!("{e:#}")})});
        }
        Ok(results)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn record(before: Observation) -> InstallRecord {
        let mut r = InstallRecord::pending(before, "a".repeat(64));
        r.official_exit = Some(0);
        r.after = Observation::Present(vec!["cable-1".into()]);
        r
    }
    #[test]
    fn incomplete_scan_can_show_presence_but_cannot_prove_origin() {
        let inventory = Inventory {
            endpoints: vec![endpoint(
                "cable-1",
                "VB-Audio Virtual Cable",
                false,
                Hardware::Software,
            )],
            enumeration_failed: true,
        };
        assert!(matches!(inventory.observation(), Observation::Present(_)));
        assert_eq!(inventory.strict_observation(), Observation::Unknown);
        assert_eq!(
            provenance(
                Some(&record(Observation::Absent)),
                &inventory.strict_observation()
            ),
            Provenance::Unknown
        );
    }
    #[test]
    fn matching_repairs_preserve_prior_app_flow_but_changed_ids_do_not() {
        let current = Observation::Present(vec!["cable-1".into()]);
        let prior = record(Observation::Absent);
        let mut repair =
            InstallRecord::pending_with_prior(current.clone(), "b".repeat(64), Some(&prior));
        assert_eq!(provenance(Some(&repair), &current), Provenance::Unknown);
        repair.official_exit = Some(3010);
        repair.after = current.clone();
        assert_eq!(
            provenance(Some(&repair), &current),
            Provenance::AppInstallObserved
        );
        let mut second =
            InstallRecord::pending_with_prior(current.clone(), "c".repeat(64), Some(&repair));
        second.official_exit = Some(0);
        second.after = current.clone();
        assert_eq!(
            provenance(Some(&second), &current),
            Provenance::AppInstallObserved
        );
        let changed = Observation::Present(vec!["changed-endpoint".into()]);
        repair.after = changed.clone();
        assert_eq!(provenance(Some(&repair), &changed), Provenance::Unknown);
        let mut replaced =
            InstallRecord::pending_with_prior(changed.clone(), "c".repeat(64), Some(&prior));
        replaced.official_exit = Some(0);
        replaced.after = changed.clone();
        assert_eq!(provenance(Some(&replaced), &changed), Provenance::Unknown);
        let mut unknown_before =
            InstallRecord::pending_with_prior(Observation::Unknown, "c".repeat(64), Some(&prior));
        unknown_before.official_exit = Some(0);
        unknown_before.after = current.clone();
        assert_eq!(
            provenance(Some(&unknown_before), &current),
            Provenance::Unknown
        );
        second.recorded_unix = 0;
        assert_eq!(provenance(Some(&second), &current), Provenance::Unknown);
    }
    #[test]
    fn recovery_errors_corrupt_marker_and_file_reads_are_conservative() {
        assert_eq!(recovery_contents(Ok(None)), Recovery::Clear);
        assert_eq!(
            recovery_contents(Err(anyhow::anyhow!("permission denied"))),
            Recovery::Unknown
        );
        assert_eq!(
            recovery_contents(Ok(Some(b"bad json".to_vec()))),
            Recovery::Invalid
        );
        assert_eq!(
            recovery_contents(Ok(Some(b"{}".to_vec()))),
            Recovery::Invalid
        );
        let bytes = br#"{"volume_id":"p","original_scalar":0.5,"source_id":"v","original_defaults":["p","p","p"]}"#.to_vec();
        assert_eq!(
            recovery_contents(Ok(Some(bytes.clone()))),
            Recovery::Pending
        );
        let data = std::env::current_dir().unwrap().join(format!(
            "evidence-fixture-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&data).unwrap();
        assert_eq!(recovery(&data), Recovery::Clear);
        assert!(read_record(&data).is_none());
        fs::write(data.join("recovery.json"), &bytes).unwrap();
        assert_eq!(recovery(&data), Recovery::Pending);
        assert_eq!(fs::read(data.join("recovery.json")).unwrap(), bytes);
        fs::write(data.join(RECORD_FILE), "{corrupt").unwrap();
        assert!(read_record(&data).is_none());
        fs::write(data.join(RECORD_FILE), vec![b' '; 65537]).unwrap();
        assert!(read_record(&data).is_none());
        let valid = record(Observation::Absent);
        fs::write(data.join(RECORD_FILE), serde_json::to_vec(&valid).unwrap()).unwrap();
        assert_eq!(
            provenance(read_record(&data).as_ref(), &valid.after),
            Provenance::AppInstallObserved
        );
        fs::write(data.join("settings.json"), "bad json").unwrap();
        assert!(configured_output(&data).is_err());
        fs::remove_dir_all(&data).unwrap();
    }
    #[test]
    fn forward_evidence_requires_absence_success_and_current_identity() {
        let current = Observation::Present(vec!["cable-1".into()]);
        let mut r = record(Observation::Absent);
        assert_eq!(
            provenance(Some(&r), &current),
            Provenance::AppInstallObserved
        );
        r.before = current.clone();
        assert_eq!(provenance(Some(&r), &current), Provenance::Preexisting);
        r.before = Observation::Unknown;
        assert_eq!(provenance(Some(&r), &current), Provenance::Unknown);
        for before in [Observation::Absent, current.clone()] {
            for code in [None, Some(1), Some(1223)] {
                let mut r = record(before.clone());
                r.official_exit = code;
                assert_eq!(provenance(Some(&r), &current), Provenance::Unknown);
            }
        }
        let mut r = record(Observation::Absent);
        r.signature_verified = false;
        assert_eq!(provenance(Some(&r), &current), Provenance::Unknown);
        let r = record(Observation::Absent);
        assert_eq!(
            provenance(Some(&r), &Observation::Absent),
            Provenance::Unknown
        );
        assert_eq!(
            provenance(Some(&r), &Observation::Present(vec!["replacement".into()])),
            Provenance::Unknown
        );
        assert_eq!(provenance(None, &current), Provenance::Unknown);
    }
    #[test]
    fn malformed_old_metadata_and_pending_records_never_prove_origin() {
        let current = Observation::Present(vec!["cable-1".into()]);
        assert!(
            serde_json::from_str::<InstallRecord>(r#"{"confirmed_pack":46,"sha256":"abc"}"#)
                .is_err()
        );
        let mut r = record(Observation::Absent);
        r.schema_version = 2;
        assert_eq!(provenance(Some(&r), &current), Provenance::Unknown);
        r.schema_version = 1;
        r.package_sha256 = "download-only".into();
        assert_eq!(provenance(Some(&r), &current), Provenance::Unknown);
        assert_eq!(
            provenance(
                Some(&InstallRecord::pending(Observation::Absent, "a".repeat(64))),
                &current
            ),
            Provenance::Unknown
        );
    }
    fn endpoint(id: &str, interface: &str, active: bool, hardware: Hardware) -> Endpoint {
        Endpoint {
            id: id.into(),
            name: format!("Renamed {id}"),
            interface: interface.into(),
            active,
            hardware,
        }
    }
    #[test]
    fn fallback_uses_active_hardware_and_ids_not_display_name_substrings() {
        let mut inventory = Inventory {
            endpoints: vec![
                endpoint("physical", "Hardware audio", true, Hardware::Physical),
                endpoint("virtual", "Voicemeeter", true, Hardware::Software),
                endpoint("cable", "VB-Audio Virtual Cable", true, Hardware::Physical),
            ],
            enumeration_failed: false,
        };
        assert!(
            matches!(fallback(&inventory, Ok(None)), Fallback::Candidate {id, ..} if id == "physical")
        );
        assert_eq!(
            fallback(&inventory, Ok(Some("cable".into()))),
            Fallback::ConfiguredInvalid
        );
        assert_eq!(
            fallback(&inventory, Ok(Some("missing".into()))),
            Fallback::ConfiguredInvalid
        );
        inventory.endpoints[0].active = false;
        assert_eq!(fallback(&inventory, Ok(None)), Fallback::Unavailable);
        inventory.endpoints[0].active = true;
        inventory.endpoints[0].hardware = Hardware::Unknown;
        assert_eq!(fallback(&inventory, Ok(None)), Fallback::Unknown);
        assert_eq!(
            fallback(&inventory, Err(anyhow::anyhow!("settings read denied"))),
            Fallback::Unknown
        );
        inventory.enumeration_failed = true;
        assert_eq!(fallback(&inventory, Ok(None)), Fallback::Unknown);
        assert_eq!(hardware_enumerator("USB"), Hardware::Physical);
        assert_eq!(hardware_enumerator("ROOT"), Hardware::Software);
        assert_eq!(hardware_enumerator("Virtual USB Audio"), Hardware::Unknown);
    }
    #[test]
    fn defaults_are_independent_and_all_roles_must_be_physical() {
        let inventory = Inventory {
            endpoints: vec![
                endpoint("p", "Physical", true, Hardware::Physical),
                endpoint("v", "VB-Audio Virtual Cable", true, Hardware::Software),
            ],
            enumeration_failed: false,
        };
        assert_eq!(
            defaults(&inventory, Ok(vec!["p".into(); 3])),
            Defaults::Physical
        );
        assert_eq!(
            defaults(&inventory, Ok(vec!["p".into(), "v".into(), "p".into()])),
            Defaults::CableRoute
        );
        assert_eq!(
            defaults(
                &inventory,
                Ok(vec!["p".into(), "missing".into(), "p".into()])
            ),
            Defaults::Invalid
        );
        assert_eq!(
            defaults(&inventory, Err(anyhow::anyhow!("default read denied"))),
            Defaults::Unknown
        );
    }
}
