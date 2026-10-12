//! Uninstall-only playback routing. No settings, endpoint volume or mute writes.
use anyhow::{ensure, Context, Result};
use crate::cable_evidence::{is_vb_cable_interface, Hardware, Inventory, Observation};

#[derive(Clone, Debug, serde::Serialize)]
pub struct Destination { pub id: String, pub name: String }
pub fn destinations(inventory: &Inventory) -> Vec<Destination> {
    if inventory.enumeration_failed { return vec![]; }
    inventory.endpoints.iter().filter(|e| e.active && e.hardware == Hardware::Physical
        && !is_vb_cable_interface(&e.interface) && !e.id.trim().is_empty())
        .map(|e| Destination {id:e.id.clone(),name:e.name.clone()}).collect()
}
pub fn preferred(inventory: &Inventory, configured: Result<Option<String>>, defaults: Result<Vec<String>>) -> Option<String> {
    let candidates = destinations(inventory);
    let eligible = |id: &String| candidates.iter().any(|e| &e.id == id);
    configured.ok().flatten().filter(eligible)
        .or_else(|| defaults.ok().and_then(|ids| ids.into_iter().find(eligible)))
        .or_else(|| candidates.first().map(|e| e.id.clone()))
}
pub(crate) trait RouteHost {
    fn inventory(&mut self) -> Result<Inventory>;
    fn default_ids(&mut self) -> Result<Vec<String>>;
    fn set_default(&mut self, id: &str, role: usize) -> Result<()>;
}
pub(crate) struct Transition { original: Vec<String>, destination: String, cable: Observation, touched: Vec<usize> }
impl Transition {
    pub fn cable_matches(&self, observation: &Observation) -> bool { &self.cable == observation }
    pub fn begin(host: &mut impl RouteHost, destination: &str) -> Result<Self> {
        let inventory = host.inventory()?;
        ensure!(destinations(&inventory).iter().any(|e| e.id == destination), "Selected playback output is no longer a verified active physical device; choose an available output and retry");
        let original = host.default_ids()?;
        ensure!(original.len() == 3 && original.iter().all(|id| inventory.endpoints.iter().any(|e| e.id == *id && e.active && (e.hardware != Hardware::Unknown || is_vb_cable_interface(&e.interface)))), "All three current playback roles must resolve to verified active endpoints");
        let cable = inventory.strict_observation();
        ensure!(matches!(cable, Observation::Present(_)), "VB-CABLE identity could not be verified before routing");
        Ok(Self {original,destination:destination.into(),cable,touched:vec![]})
    }
    pub fn route(&mut self, host: &mut impl RouteHost) -> Result<()> {
        for role in 0..3 {
            let current = host.default_ids()?;
            ensure!(current.len() == 3 && current.iter().enumerate().all(|(r,id)|
                if self.touched.contains(&r) { *id == self.destination }
                else { *id == self.original[r] || (!self.touched.is_empty() && *id == self.destination) }),
                "Windows playback routes changed concurrently; retry after reviewing the outputs");
            // Windows can move console and multimedia together after one setter.
            for (r,id) in current.iter().enumerate() {
                if *id == self.destination && self.original[r] != self.destination && !self.touched.contains(&r) { self.touched.push(r); }
            }
            let inventory = host.inventory()?;
            ensure!(inventory.strict_observation() == self.cable && destinations(&inventory).iter().any(|e| e.id == self.destination), "Cable or selected output identity changed before routing");
            if current[role] != self.destination {
                // Track before the call: a failed COM call may have applied its change.
                self.touched.push(role);
                host.set_default(&self.destination,role).with_context(|| format!("Could not switch Windows playback role {role}"))?;
            }
        }
        self.verify(host)
    }
    pub fn verify(&self, host: &mut impl RouteHost) -> Result<()> {
        ensure!(destinations(&host.inventory()?).iter().any(|e| e.id == self.destination), "Selected playback output became unavailable");
        ensure!(host.default_ids()? == vec![self.destination.clone();3], "Windows did not confirm the selected physical output for all three playback roles");
        Ok(())
    }
    pub fn rollback(&self, host: &mut impl RouteHost) -> Result<()> {
        if self.touched.is_empty() { return Ok(()); }
        let inventory = host.inventory()?;
        ensure!(inventory.strict_observation() == self.cable && matches!(self.cable, Observation::Present(_)), "Previous routes were not restored because cable presence or identity is uncertain");
        ensure!(self.original.iter().all(|id| inventory.endpoints.iter().any(|e| e.id == *id && e.active && (e.hardware != Hardware::Unknown || is_vb_cable_interface(&e.interface)))), "Previous playback endpoints are no longer active; the physical output was retained");
        let initial = host.default_ids()?;
        ensure!(initial.len() == 3, "Cannot verify all playback roles for rollback");
        let restore: Vec<bool> = (0..3).map(|r| initial[r] == self.destination && self.original[r] != self.destination).collect();
        let mut expected = initial.clone();
        for role in 0..3 {
            if restore[role] { expected[role] = self.original[role].clone(); }
        }
        let mut observed = initial;
        let mut failures = vec![];
        // Console first: it may also move multimedia. Restore multimedia afterward.
        for role in 0..3 {
            let current = host.default_ids()?;
            ensure!(current.len() == 3, "Cannot verify playback roles during rollback");
            // Changes since our last observation belong to Windows/the user.
            for r in 0..3 {
                if current[r] != observed[r] { expected[r] = current[r].clone(); }
            }
            if current[role] == expected[role] { observed = current; continue; }
            let requested = expected[role].clone();
            if let Err(e) = host.set_default(&requested,role) {failures.push(format!("role {role}: {e:#}"));}
            let after = host.default_ids()?;
            ensure!(after.len() == 3, "Cannot verify playback roles after rollback write");
            // A setter may propagate its target to another role. Its intended
            // route still needs restoring even if that role was initially untouched.
            for r in 0..3 {
                if r != role && after[r] != current[r] && after[r] != requested {
                    expected[r] = after[r].clone();
                }
            }
            observed = after;
        }
        ensure!(failures.is_empty(), "Playback rollback failed: {}", failures.join("; "));
        ensure!(host.default_ids()? == expected, "Windows did not confirm the restored playback roles");
        Ok(())
    }
}

windows::core::define_interface!(PolicyConfig, PolicyConfigVtbl, 0xf8679f50_850a_41cf_9c72_430f290290c8);
#[repr(C)]
pub struct PolicyConfigVtbl {
    pub base__: windows::core::IUnknown_Vtbl,
    pub reserved: [usize;10],
    pub set_default_endpoint: unsafe extern "system" fn(*mut std::ffi::c_void, windows::core::PCWSTR, i32) -> windows::core::HRESULT,
    pub set_visibility: usize,
}
pub fn set_native(id: &str, role: usize) -> Result<()> {
    use windows::{core::{Interface, IUnknown, GUID, PCWSTR}, Win32::System::Com::{CoCreateInstance, CLSCTX_ALL}};
    ensure!(role < 3 && !id.is_empty(), "Invalid playback route");
    let _com = crate::platform::Com::new()?;
    let unknown: IUnknown = unsafe {CoCreateInstance(&GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9),None,CLSCTX_ALL)?};
    let policy: PolicyConfig = unknown.cast()?;
    let wide: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
    unsafe {(policy.vtable().set_default_endpoint)(policy.as_raw(),PCWSTR(wide.as_ptr()),role as i32).ok()?;}
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{cable::{ActionSnapshot, RemovalHost},cable_evidence::{Endpoint, Defaults, Recovery},official_cable::LaunchResult};
    pub struct Mock {
        pub inventory: Inventory, pub ids: Vec<String>, pub writes: Vec<(String,usize)>,
        pub fail_role: Option<usize>, pub prepare_ok: bool, pub running: bool, pub recovery: Recovery,
        pub launch: LaunchResult, pub after: Option<Observation>, pub launches: usize,
        pub volumes: Vec<(f32,bool)>, pub paired_console: bool,
    }
    impl Mock {
        pub fn new() -> Self {
            Self {inventory:Inventory {endpoints:vec![
                Endpoint {id:"cable".into(),name:"CABLE".into(),interface:"VB-Audio Virtual Cable".into(),active:true,hardware:Hardware::Unknown},
                Endpoint {id:"physical".into(),name:"Speakers".into(),interface:"Physical".into(),active:true,hardware:Hardware::Physical},
                Endpoint {id:"second".into(),name:"Headphones".into(),interface:"Physical".into(),active:true,hardware:Hardware::Physical},
                Endpoint {id:"software".into(),name:"FxSound".into(),interface:"Software".into(),active:true,hardware:Hardware::Software}],enumeration_failed:false},
                ids:vec!["cable".into();3],writes:vec![],fail_role:None,prepare_ok:true,running:false,recovery:Recovery::Clear,
                launch:LaunchResult::Exit(0),after:Some(Observation::Absent),launches:0,volumes:vec![(0.42,true),(0.83,false)],paired_console:false}
        }
    }
    impl RouteHost for Mock {
        fn inventory(&mut self) -> Result<Inventory> {Ok(self.inventory.clone())}
        fn default_ids(&mut self) -> Result<Vec<String>> {Ok(self.ids.clone())}
        fn set_default(&mut self,id:&str,role:usize) -> Result<()> {
            self.writes.push((id.into(),role));self.ids[role]=id.into();
            if self.paired_console && role == 0 { self.ids[1]=id.into(); }
            if self.fail_role == Some(role) {self.fail_role=None;anyhow::bail!("injected route error after mutation");} Ok(())
        }
    }
    impl RemovalHost for Mock {
        fn pending(&self) -> bool {false}
        fn snapshot(&mut self) -> Result<ActionSnapshot> {
            Ok(ActionSnapshot {observation:self.inventory.strict_observation(),fallback:crate::cable_evidence::Fallback::Candidate {id:"physical".into(),name:"Speakers".into()},defaults:crate::cable_evidence::defaults(&self.inventory,Ok(self.ids.clone())),recovery:self.recovery,app_running:self.running})
        }
        fn prepare(&mut self) -> Result<()> {ensure!(self.prepare_ok,"injected download failure");Ok(())}
        fn launch(&mut self) -> LaunchResult {
            assert_eq!(self.ids,vec!["physical".to_string();3]);self.launches+=1;
            match &self.after {
                Some(Observation::Absent) => self.inventory.endpoints.retain(|e| e.id != "cable"),
                Some(Observation::Unknown) => self.inventory.enumeration_failed=true,
                _ => (),
            }
            self.launch
        }
    }
    #[test] fn selection_prefers_eligible_configured_then_current_then_first_and_excludes_virtual_unknown_stale() {
        let mut host=Mock::new();
        assert_eq!(destinations(&host.inventory).iter().map(|e|e.id.as_str()).collect::<Vec<_>>(),["physical","second"]);
        assert_eq!(preferred(&host.inventory,Ok(Some("second".into())),Ok(host.ids.clone())),Some("second".into()));
        assert_eq!(preferred(&host.inventory,Ok(Some("missing".into())),Ok(vec!["second".into();3])),Some("second".into()));
        assert_eq!(preferred(&host.inventory,Err(anyhow::anyhow!("settings denied")),Ok(host.ids.clone())),Some("physical".into()));
        for id in ["missing","software","cable"] {assert!(Transition::begin(&mut host,id).is_err());}
        host.inventory.endpoints[1].active=false;assert!(Transition::begin(&mut host,"physical").is_err());
        host.inventory.endpoints[1].active=true;host.inventory.endpoints[1].hardware=Hardware::Unknown;assert!(Transition::begin(&mut host,"physical").is_err());
        host.inventory.enumeration_failed=true;assert!(destinations(&host.inventory).is_empty());assert!(host.writes.is_empty());
    }
    #[test] fn partial_route_failure_restores_exact_roles_without_changing_volume_or_mute() {
        let mut host=Mock::new();host.ids=["cable","software","second"].map(str::to_string).into();
        let original=host.ids.clone();let volume=host.volumes.clone();host.fail_role=Some(1);
        let mut transition=Transition::begin(&mut host,"physical").unwrap();
        assert!(transition.route(&mut host).is_err());transition.rollback(&mut host).unwrap();
        assert_eq!(host.ids,original);assert_eq!(host.volumes,volume);
    }
    #[test] fn software_originals_route_and_rollback_but_absent_unknown_or_stale_cable_never_restores() {
        for after in [0,1,2,3] {
            let mut host=Mock::new();host.ids=vec!["software".into();3];
            let mut transition=Transition::begin(&mut host,"physical").unwrap();transition.route(&mut host).unwrap();
            match after {1=>host.inventory.endpoints.retain(|e|e.id!="cable"),2=>host.inventory.enumeration_failed=true,3=>host.inventory.endpoints[0].id="replacement".into(),_=>()}
            let result=transition.rollback(&mut host);
            if after == 0 {assert!(result.is_ok());assert_eq!(host.ids,vec!["software".to_string();3]);}
            else {assert!(result.is_err());assert_eq!(host.ids,vec!["physical".to_string();3]);}
        }
    }
    #[test] fn concurrent_route_is_preserved_by_rollback() {
        let mut host=Mock::new();let mut transition=Transition::begin(&mut host,"physical").unwrap();transition.route(&mut host).unwrap();
        host.ids[1]="second".into();transition.rollback(&mut host).unwrap();
        assert_eq!(host.ids,["cable","second","cable"]);
    }
    #[test] fn windows_paired_roles_and_partial_native_failure_restore_exact_split_roles() {
        for fail in [None,Some(0)] {
            let mut host=Mock::new();host.paired_console=true;host.fail_role=fail;
            host.ids=["cable","software","second"].map(str::to_string).into();
            let original=host.ids.clone();let volume=host.volumes.clone();
            let mut transition=Transition::begin(&mut host,"physical").unwrap();
            let result=transition.route(&mut host);
            assert_eq!(result.is_err(),fail.is_some());
            if result.is_ok(){assert_eq!(host.ids,vec!["physical".to_string();3]);}
            transition.rollback(&mut host).unwrap();
            assert_eq!(host.ids,original);assert_eq!(host.volumes,volume);
        }
    }
    #[test] fn paired_rollback_preserves_untouched_and_concurrently_selected_multimedia() {
        for fail in [None,Some(0)] {
            let mut host=Mock::new();host.paired_console=true;host.fail_role=fail;
            host.ids=["cable","physical","software"].map(str::to_string).into();
            let original=host.ids.clone();let volume=host.volumes.clone();
            let mut transition=Transition::begin(&mut host,"physical").unwrap();
            let _=transition.route(&mut host);
            transition.rollback(&mut host).unwrap();
            assert_eq!(host.ids,original);assert_eq!(host.volumes,volume);
        }
        let mut host=Mock::new();host.paired_console=true;
        let mut transition=Transition::begin(&mut host,"physical").unwrap();transition.route(&mut host).unwrap();
        host.ids[1]="second".into();
        transition.rollback(&mut host).unwrap();
        assert_eq!(host.ids,["cable","second","cable"]);
    }
    #[test] fn verified_transition_requires_all_three_selected_roles() {
        let mut host=Mock::new();let mut transition=Transition::begin(&mut host,"physical").unwrap();transition.route(&mut host).unwrap();
        assert_eq!(host.writes.len(),3);host.ids[2]="second".into();assert!(transition.verify(&mut host).is_err());
        host.inventory.endpoints[1].active=false;assert!(transition.verify(&mut host).is_err());
        assert_eq!(crate::cable_evidence::defaults(&host.inventory,Ok(host.ids)),Defaults::Invalid);
    }
}
