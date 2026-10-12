use anyhow::Result;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    Present,
    Absent,
    Unknown,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct UninstallOptions {
    pub delete_data: bool,
    pub remove_cable: bool,
    pub english: bool,
}

// An exact interface identity avoids treating Voicemeeter, generic virtual devices,
// or a user-renamed endpoint containing "cable" as VB-CABLE. The interface property
// also survives edits to an endpoint's display name. This establishes presence only.
pub fn is_vb_cable_interface(name: &str) -> bool {
    crate::cable_evidence::is_vb_cable_interface(name)
}

pub fn classify(endpoints: impl IntoIterator<Item = Result<String>>) -> Presence {
    let mut failed = false;
    for endpoint in endpoints {
        match endpoint {
            Ok(name) if is_vb_cable_interface(&name) => return Presence::Present,
            Ok(_) => (),
            Err(_) => failed = true,
        }
    }
    if failed {
        Presence::Unknown
    } else {
        Presence::Absent
    }
}

pub fn detect() -> Presence {
    match crate::cable_evidence::observe_native() {
        crate::cable_evidence::Observation::Present(_) => Presence::Present,
        crate::cable_evidence::Observation::Absent => Presence::Absent,
        crate::cable_evidence::Observation::Unknown => Presence::Unknown,
    }
}

#[derive(Debug, serde::Serialize)]
pub struct Review {
    pub presence: i32,
    pub provenance: i32,
    pub fallback: i32,
    pub fallback_name: String,
    pub defaults: i32,
    pub recovery: i32,
    pub outputs: Vec<crate::cable_routing::Destination>,
    pub preferred_output: Option<String>,
}
pub fn review(data: &std::path::Path) -> Review {
    use crate::cable_evidence as evidence;
    let inventory = evidence::native_inventory(true).unwrap_or(evidence::Inventory {endpoints: vec![], enumeration_failed: true});
    review_inventory(data, &inventory, evidence::native_defaults())
}
pub fn review_inventory(data: &std::path::Path, inventory: &crate::cable_evidence::Inventory, default_ids: Result<Vec<String>>) -> Review {
    use crate::cable_evidence as evidence;
    let observation = inventory.observation();
    let preferred_output = crate::cable_routing::preferred(inventory, evidence::configured_output(data), default_ids.as_ref().map(Clone::clone).map_err(|e| anyhow::anyhow!("{e:#}")));
    let outputs = crate::cable_routing::destinations(inventory);
    let record = evidence::read_record(data);
    let provenance = match evidence::provenance(record.as_ref(), &inventory.strict_observation()) {
        evidence::Provenance::Preexisting => 1,
        evidence::Provenance::AppInstallObserved => 2,
        evidence::Provenance::Unknown => 3,
    };
    let (fallback, fallback_name) = match evidence::fallback(inventory, evidence::configured_output(data)) {
        evidence::Fallback::Candidate {name, ..} => (1, name),
        evidence::Fallback::Unavailable => (2, String::new()),
        evidence::Fallback::Unknown => (3, String::new()),
        evidence::Fallback::ConfiguredInvalid => (4, String::new()),
    };
    let defaults = match evidence::defaults(inventory, default_ids) {
        evidence::Defaults::Physical => 1, evidence::Defaults::CableRoute => 2,
        evidence::Defaults::Invalid => 3, evidence::Defaults::Unknown => 4,
    };
    let recovery = match evidence::recovery(data) {
        evidence::Recovery::Clear => 1, evidence::Recovery::Pending => 2,
        evidence::Recovery::Invalid => 3, evidence::Recovery::Unknown => 4,
    };
    Review {presence: match observation {evidence::Observation::Present(_) => 1, evidence::Observation::Absent => 2, evidence::Observation::Unknown => 3}, provenance, fallback, fallback_name, defaults, recovery, outputs, preferred_output}
}

/// Independent cable result; only these three states permit app-file removal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum Outcome {
    Kept = 1, AwaitingOfficial = 2, AlreadyAbsent = 3, ConfirmedAbsent = 4,
    RestartRequired = 5, Cancelled = 6, LaunchFailed = 7, ExitFailed = 8,
    StillPresent = 9, UnknownAfter = 10, PreflightBlocked = 11,
    PackageFailed = 12, SetupStillRunning = 13, PreparingOfficial = 14, RoutingOutput = 15,
}
impl Outcome {
    pub fn permits_app_removal(self) -> bool {matches!(self, Self::Kept | Self::AlreadyAbsent | Self::ConfirmedAbsent)}
    pub fn message(self, english: bool) -> &'static str {
        match (self, english) {
            (Self::RoutingOutput,true) => "Switching and verifying all Windows playback outputs before cable removal…",
            (Self::RoutingOutput,false) => "در حال تغییر و تأیید همهٔ خروجی‌های پخش ویندوز پیش از حذف کابل…",
            (Self::PreparingOfficial,true) => "Preparing and verifying the official VB-CABLE package…",
            (Self::PreparingOfficial,false) => "در حال آماده‌سازی و تأیید بستهٔ رسمی VB-CABLE…",
            (Self::Kept,true) => "No cable removal was requested; its current state was left unchanged.",
            (Self::Kept,false) => "حذف کابل درخواست نشد؛ وضعیت فعلی آن تغییر نکرد.",
            (Self::AwaitingOfficial,true) => "In the official VB-CABLE window, choose Remove Driver. Other apps may use this cable.",
            (Self::AwaitingOfficial,false) => "در پنجرهٔ رسمی VB-CABLE، گزینهٔ Remove Driver را بزن. ممکن است برنامه‌های دیگر از این کابل استفاده کنند.",
            (Self::AlreadyAbsent,true) => "VB-CABLE endpoints were already absent; no driver setup was launched.",
            (Self::AlreadyAbsent,false) => "خروجی‌های VB-CABLE از قبل شناسایی نشدند؛ نصاب درایور اجرا نشد.",
            (Self::ConfirmedAbsent,true) => "Official setup finished and VB-CABLE endpoints are absent. Restart Windows as recommended by VB-Audio.",
            (Self::ConfirmedAbsent,false) => "نصاب رسمی پایان یافت و خروجی‌های VB-CABLE شناسایی نمی‌شوند. طبق توصیهٔ VB-Audio ویندوز را بازراه‌اندازی کن.",
            (Self::RestartRequired,true) => "VB-CABLE setup requires a restart. The app and its data were kept; restart Windows, then retry.",
            (Self::RestartRequired,false) => "نصاب VB-CABLE به بازراه‌اندازی نیاز دارد. برنامه و داده‌ها حفظ شدند؛ ویندوز را بازراه‌اندازی و دوباره تلاش کن.",
            (Self::Cancelled,true) => "Administrator approval was cancelled. The app and its data were kept.",
            (Self::Cancelled,false) => "اجازهٔ مدیر لغو شد. برنامه و داده‌های آن حفظ شدند.",
            (Self::LaunchFailed,true) => "Official VB-CABLE setup could not start. The app and its data were kept.",
            (Self::LaunchFailed,false) => "نصاب رسمی VB-CABLE اجرا نشد. برنامه و داده‌های آن حفظ شدند.",
            (Self::ExitFailed,true) => "Official VB-CABLE setup reported failure. The app and its data were kept.",
            (Self::ExitFailed,false) => "نصاب رسمی VB-CABLE شکست را گزارش کرد. برنامه و داده‌های آن حفظ شدند.",
            (Self::StillPresent,true) => "VB-CABLE is still present. Removal was not confirmed; the app and its data were kept.",
            (Self::StillPresent,false) => "کابل VB-CABLE هنوز وجود دارد. حذف تأیید نشد؛ برنامه و داده‌های آن حفظ شدند.",
            (Self::UnknownAfter,true) => "VB-CABLE removal could not be verified. The app and its data were kept.",
            (Self::UnknownAfter,false) => "حذف VB-CABLE قابل تأیید نبود. برنامه و داده‌های آن حفظ شدند.",
            (Self::PreflightBlocked,true) => "Cable removal is blocked. Complete Stop/Exit and choose a verified active physical playback output.",
            (Self::PreflightBlocked,false) => "حذف کابل متوقف شد. توقف/خروج را کامل کن و یک خروجی پخش فیزیکی فعال و تأییدشده انتخاب کن.",
            (Self::PackageFailed,true) => "The official VB-CABLE package could not be prepared or verified. The app and its data were kept.",
            (Self::PackageFailed,false) => "بستهٔ رسمی VB-CABLE آماده یا تأیید نشد. برنامه و داده‌های آن حفظ شدند.",
            (Self::SetupStillRunning,true) => "Official setup is still running or its completion is uncertain. Finish that window before retrying. The app and data were kept.",
            (Self::SetupStillRunning,false) => "نصاب رسمی هنوز باز است یا پایان آن مشخص نیست. پیش از تلاش دوباره، آن پنجره را به پایان برسان. برنامه و داده‌ها حفظ شدند.",
        }
    }
}
pub fn retry_message(english: bool) -> &'static str {
    if english {"Clear ‘Remove virtual cable’ to keep the cable and remove only MAXI SOUNDSET."}
    else {"برای حفظ کابل و حذف فقط MAXI SOUNDSET، تیک «حذف کابل مجازی» را بردار."}
}
#[derive(Clone, Debug)]
pub(crate) struct ActionSnapshot {
    pub observation: crate::cable_evidence::Observation,
    pub fallback: crate::cable_evidence::Fallback,
    pub defaults: crate::cable_evidence::Defaults,
    pub recovery: crate::cable_evidence::Recovery,
    pub app_running: bool,
}
impl ActionSnapshot {
    fn ready(&self) -> bool {
        use crate::cable_evidence::*;
        !self.app_running && !matches!(self.observation, Observation::Unknown)
            && matches!(self.fallback, Fallback::Candidate {..})
            && self.defaults == Defaults::Physical && self.recovery == Recovery::Clear
    }
}
/// Injection is internal for isolated tests; production API always constructs NativeRemoval.
pub(crate) trait RemovalHost {
    fn pending(&self) -> bool;
    fn snapshot(&mut self) -> Result<ActionSnapshot>;
    fn prepare(&mut self) -> Result<()>;
    fn launch(&mut self) -> crate::official_cable::LaunchResult;
}
pub(crate) fn run_removal(host: &mut impl RemovalHost, selected: bool, mut report: impl FnMut(Outcome)) -> Outcome {
    use crate::{cable_evidence::Observation, official_cable::LaunchResult};
    let outcome = if host.pending() {Outcome::SetupStillRunning}
    else if !selected {Outcome::Kept}
    else {
        let first = host.snapshot();
        match first {
            Ok(first) if first.ready() => {
                if first.observation == Observation::Absent {Outcome::AlreadyAbsent}
                else if { report(Outcome::PreparingOfficial); host.prepare().is_err() } {Outcome::PackageFailed}
                else {
                    // Download/UAC preparation can take time: never use the review snapshot.
                    match host.snapshot() {
                        Ok(fresh) if fresh.ready() && fresh.observation == first.observation => {
                            report(Outcome::AwaitingOfficial);
                            match host.launch() {
                                LaunchResult::Cancelled => Outcome::Cancelled,
                                LaunchResult::Failed => Outcome::LaunchFailed,
                                LaunchResult::StillRunning => Outcome::SetupStillRunning,
                                LaunchResult::Exit(3010) => Outcome::RestartRequired,
                                LaunchResult::Exit(0) => match host.snapshot() {
                                    Ok(after) if after.observation == Observation::Absent && after.ready() => Outcome::ConfirmedAbsent,
                                    Ok(after) if matches!(after.observation, Observation::Present(_)) => Outcome::StillPresent,
                                    Ok(after) if after.observation == Observation::Absent => Outcome::PreflightBlocked,
                                    _ => Outcome::UnknownAfter,
                                },
                                LaunchResult::Exit(_) => Outcome::ExitFailed,
                            }
                        }
                        _ => Outcome::PreflightBlocked,
                    }
                }
            }
            _ => Outcome::PreflightBlocked,
        }
    };
    report(outcome);
    outcome
}
/// Destination is transient explicit uninstall consent, never an app setting.
pub(crate) fn run_removal_to_output(host: &mut (impl RemovalHost + crate::cable_routing::RouteHost), selected: bool, destination: &str, mut report: impl FnMut(Outcome)) -> (Outcome, Option<String>) {
    use crate::{cable_evidence::{Observation, Recovery, Defaults}, official_cable::LaunchResult, cable_routing::Transition};
    let mut transition = None;
    let mut failure_outcome = Outcome::PreflightBlocked;
    let action = (|| -> Result<Outcome> {
        if host.pending() { return Ok(Outcome::SetupStillRunning); }
        if !selected { return Ok(Outcome::Kept); }
        let first = host.snapshot()?;
        anyhow::ensure!(!first.app_running && first.recovery == Recovery::Clear, "App shutdown / audio recovery must finish before changing playback output");
        anyhow::ensure!(!matches!(first.observation, Observation::Unknown), "VB-CABLE identity could not be verified");
        if first.observation == Observation::Absent { return Ok(Outcome::AlreadyAbsent); }
        transition = Some(Transition::begin(host,destination)?);
        anyhow::ensure!(transition.as_ref().unwrap().cable_matches(&first.observation), "VB-CABLE identity changed before playback routing");
        report(Outcome::RoutingOutput);
        transition.as_mut().unwrap().route(host)?;
        failure_outcome = Outcome::PackageFailed;
        report(Outcome::PreparingOfficial);
        host.prepare()?;
        failure_outcome = Outcome::PreflightBlocked;
        let fresh = host.snapshot()?;
        anyhow::ensure!(!fresh.app_running && fresh.recovery == Recovery::Clear && fresh.observation == first.observation && fresh.defaults == Defaults::Physical, "App, recovery, cable identity or playback routes changed while preparing official setup");
        transition.as_ref().unwrap().verify(host)?;
        report(Outcome::AwaitingOfficial);
        Ok(match host.launch() {
            LaunchResult::Cancelled => Outcome::Cancelled,
            LaunchResult::Failed => Outcome::LaunchFailed,
            LaunchResult::StillRunning => Outcome::SetupStillRunning,
            LaunchResult::Exit(3010) => Outcome::RestartRequired,
            LaunchResult::Exit(0) => match host.snapshot() {
                Ok(after) if after.observation == Observation::Absent && !after.app_running && after.recovery == Recovery::Clear && transition.as_ref().unwrap().verify(host).is_ok() => Outcome::ConfirmedAbsent,
                Ok(after) if matches!(after.observation, Observation::Present(_)) => Outcome::StillPresent,
                Ok(after) if after.observation == Observation::Absent => Outcome::PreflightBlocked,
                _ => Outcome::UnknownAfter,
            },
            LaunchResult::Exit(_) => Outcome::ExitFailed,
        })
    })();
    let (outcome,mut detail) = match action {Ok(outcome)=>(outcome,None),Err(e)=>(failure_outcome,Some(format!("{e:#}")))};
    // Only definite early/cancel/failure outcomes permit rollback. Unknown, running,
    // restart, missing/replaced cable or successful removal retain the safe route.
    if matches!(outcome, Outcome::PackageFailed | Outcome::PreflightBlocked | Outcome::Cancelled | Outcome::LaunchFailed | Outcome::ExitFailed | Outcome::StillPresent) {
        if let Some(transition) = transition {
            let rollback = (|| -> Result<()> {
                let state = host.snapshot()?;
                anyhow::ensure!(!state.app_running && state.recovery == Recovery::Clear, "Playback rollback skipped because app or recovery state changed");
                transition.rollback(host)
            })();
            if let Err(e) = rollback { detail = Some(format!("{}{}Playback output retained / rollback status: {e:#}", detail.as_deref().unwrap_or(""), if detail.is_some() {"; "} else {""})); }
        }
    }
    report(outcome);
    (outcome,detail)
}
pub(crate) struct NativeRemoval<'a> {root: &'a std::path::Path, folder: Option<std::path::PathBuf>, installer: Option<std::path::PathBuf>, retain_package: bool}
impl<'a> NativeRemoval<'a> {
    pub fn new(root: &'a std::path::Path) -> Self {Self {root, folder: None, installer: None, retain_package: false}}
}
impl Drop for NativeRemoval<'_> {
    fn drop(&mut self) {
        if !self.retain_package {if let Some(folder) = &self.folder {let _ = std::fs::remove_dir_all(folder);}}
    }
}
impl RemovalHost for NativeRemoval<'_> {
    fn pending(&self) -> bool {crate::official_cable::setup_still_running()}
    fn snapshot(&mut self) -> Result<ActionSnapshot> {
        use crate::cable_evidence as e;
        crate::operations::validate_removal_root(self.root)?;
        let inventory = e::native_inventory(true)?;
        let data = self.root.join("Data");
        Ok(ActionSnapshot {observation: inventory.strict_observation(), fallback: e::fallback(&inventory, e::configured_output(&data)), defaults: e::defaults(&inventory, e::native_defaults()), recovery: e::recovery(&data), app_running: crate::running_app::is_running(self.root)?})
    }
    fn prepare(&mut self) -> Result<()> {
        // Reuse only a fully validated signed cache; never mutate app Data here.
        if let Some(package) = crate::official_cable::cached_package(&self.root.join("Data")) {
            self.installer = Some(package.installer);
            return Ok(());
        }
        let folder = std::env::temp_dir().join(format!("MAXI-VBCABLE-Remove-{}", crate::operations::nonce()));
        std::fs::create_dir(&folder)?;
        self.folder = Some(folder.clone());
        let package = crate::official_cable::acquire_package(&folder, false, |_|{})?;
        self.installer = Some(package.installer);
        Ok(())
    }
    fn launch(&mut self) -> crate::official_cable::LaunchResult {
        let result = self.installer.as_ref().map(|p| crate::official_cable::launch_official(p)).unwrap_or(crate::official_cable::LaunchResult::Failed);
        self.retain_package = result == crate::official_cable::LaunchResult::StillRunning;
        result
    }
}
impl crate::cable_routing::RouteHost for NativeRemoval<'_> {
    fn inventory(&mut self) -> Result<crate::cable_evidence::Inventory> {crate::cable_evidence::native_inventory(true)}
    fn default_ids(&mut self) -> Result<Vec<String>> {crate::cable_evidence::native_defaults()}
    fn set_default(&mut self, id: &str, role: usize) -> Result<()> {
        crate::operations::validate_removal_root(self.root)?;
        anyhow::ensure!(!crate::running_app::is_running(self.root)?, "App restarted before playback routing; close it and retry");
        crate::cable_routing::set_native(id,role)
    }
}


pub fn outcome_message(code: i32, english: bool) -> &'static str {
    let outcome = match code {
        1 => Outcome::Kept, 2 => Outcome::AwaitingOfficial, 3 => Outcome::AlreadyAbsent,
        4 => Outcome::ConfirmedAbsent, 5 => Outcome::RestartRequired, 6 => Outcome::Cancelled,
        7 => Outcome::LaunchFailed, 8 => Outcome::ExitFailed, 9 => Outcome::StillPresent,
        10 => Outcome::UnknownAfter, 11 => Outcome::PreflightBlocked, 12 => Outcome::PackageFailed,
        13 => Outcome::SetupStillRunning, 14 => Outcome::PreparingOfficial, 15 => Outcome::RoutingOutput, _ => return "",
    };
    outcome.message(english)
}
pub fn restart_recommended(previous: bool, outcome: Outcome) -> bool {
    previous || matches!(outcome, Outcome::ConfirmedAbsent | Outcome::RestartRequired)
}
#[cfg(test)]
mod un04_tests {
    use super::*;
    use crate::{cable_evidence::{Observation, Fallback, Defaults, Recovery}, official_cable::LaunchResult};
    use std::collections::VecDeque;
    fn ready(observation: Observation) -> ActionSnapshot {
        ActionSnapshot {observation, fallback: Fallback::Candidate {id: "physical".into(),name:"Physical".into()}, defaults: Defaults::Physical, recovery: Recovery::Clear, app_running:false}
    }
    fn present() -> Observation {Observation::Present(vec!["vb-endpoint".into()])}
    struct Mock {snapshots: VecDeque<Result<ActionSnapshot>>, events: Vec<&'static str>, pending: bool, prepare_ok: bool, launch: LaunchResult}
    impl Mock {
        fn new(before: ActionSnapshot, after: ActionSnapshot, launch: LaunchResult) -> Self { Self {snapshots: [Ok(before.clone()),Ok(before),Ok(after)].into(),events:vec![],pending:false,prepare_ok:true,launch} }
    }
    impl RemovalHost for Mock {
        fn pending(&self) -> bool {self.pending}
        fn snapshot(&mut self) -> Result<ActionSnapshot> {self.events.push("snapshot");self.snapshots.pop_front().expect("unexpected snapshot")}
        fn prepare(&mut self) -> Result<()> {self.events.push("prepare");anyhow::ensure!(self.prepare_ok,"mock package failure");Ok(())}
        fn launch(&mut self) -> LaunchResult {self.events.push("launch");self.launch}
    }
    #[test] fn keep_default_independent_and_inflight_blocks_new_actions() {
        let defaults=UninstallOptions::default(); assert!(!defaults.remove_cable && !defaults.delete_data);
        let mut host=Mock::new(ready(present()),ready(Observation::Absent),LaunchResult::Exit(0));
        assert_eq!(run_removal(&mut host,false, |_|{}),Outcome::Kept); assert!(host.events.is_empty());
        host.pending=true;
        for selected in [false,true] {assert_eq!(run_removal(&mut host,selected, |_|{}),Outcome::SetupStillRunning);}
        assert!(host.events.is_empty());
    }
    #[test] fn absent_never_launches_unknown_or_unsafe_never_prepares() {
        let mut absent=Mock::new(ready(Observation::Absent),ready(Observation::Absent),LaunchResult::Exit(0));
        assert_eq!(run_removal(&mut absent,true, |_|{}),Outcome::AlreadyAbsent); assert_eq!(absent.events,["snapshot"]);
        let mut cases=vec![ready(Observation::Unknown)];
        let mut running=ready(present()); running.app_running=true; cases.push(running);
        for fallback in [Fallback::Unavailable,Fallback::Unknown,Fallback::ConfiguredInvalid] {let mut s=ready(present());s.fallback=fallback;cases.push(s);}
        for defaults in [Defaults::CableRoute,Defaults::Invalid,Defaults::Unknown] {let mut s=ready(present());s.defaults=defaults;cases.push(s);}
        for recovery in [Recovery::Pending,Recovery::Invalid,Recovery::Unknown] {let mut s=ready(present());s.recovery=recovery;cases.push(s);}
        for before in cases {let mut host=Mock::new(before,ready(Observation::Absent),LaunchResult::Exit(0));assert_eq!(run_removal(&mut host,true, |_|{}),Outcome::PreflightBlocked);assert_eq!(host.events,["snapshot"]);}
        let mut read_error=Mock::new(ready(present()),ready(Observation::Absent),LaunchResult::Exit(0)); read_error.snapshots[0]=Err(anyhow::anyhow!("host read failure"));
        assert_eq!(run_removal(&mut read_error,true, |_|{}),Outcome::PreflightBlocked);
    }
    #[test] fn download_failure_or_fresh_identity_route_recovery_change_blocks_launch() {
        let mut failed=Mock::new(ready(present()),ready(Observation::Absent),LaunchResult::Exit(0));failed.prepare_ok=false;
        assert_eq!(run_removal(&mut failed,true, |_|{}),Outcome::PackageFailed);assert_eq!(failed.events,["snapshot","prepare"]);
        let mut changes=vec![ready(Observation::Absent),ready(Observation::Unknown),ready(Observation::Present(vec!["different".into()]))];
        let mut route=ready(present());route.defaults=Defaults::CableRoute;changes.push(route);
        let mut recovery=ready(present());recovery.recovery=Recovery::Pending;changes.push(recovery);
        let mut running=ready(present());running.app_running=true;changes.push(running);
        for changed in changes {let mut host=Mock::new(ready(present()),ready(Observation::Absent),LaunchResult::Exit(0));host.snapshots[1]=Ok(changed);assert_eq!(run_removal(&mut host,true, |_|{}),Outcome::PreflightBlocked);assert_eq!(host.events,["snapshot","prepare","snapshot"]);}
    }
    #[test] fn official_cancel_failure_restart_and_uncertain_process_preserve_app_permission() {
        for (launch,expected) in [(LaunchResult::Cancelled,Outcome::Cancelled),(LaunchResult::Failed,Outcome::LaunchFailed),(LaunchResult::Exit(5),Outcome::ExitFailed),(LaunchResult::Exit(3010),Outcome::RestartRequired),(LaunchResult::StillRunning,Outcome::SetupStillRunning)] {
            let mut host=Mock::new(ready(present()),ready(Observation::Absent),launch);let mut reports=vec![];
            let result=run_removal(&mut host,true,|event|reports.push(event)); assert_eq!(result,expected);assert!(!result.permits_app_removal());
            assert_eq!(host.events,["snapshot","prepare","snapshot","launch"]);assert_eq!(reports,[Outcome::PreparingOfficial,Outcome::AwaitingOfficial,expected]);
        }
    }
    #[test] fn zero_exit_needs_fresh_absence_and_post_audio_readiness() {
        let mut cases=vec![(ready(present()),Outcome::StillPresent),(ready(Observation::Unknown),Outcome::UnknownAfter),(ready(Observation::Absent),Outcome::ConfirmedAbsent)];
        for defaults in [Defaults::CableRoute,Defaults::Invalid,Defaults::Unknown] {let mut s=ready(Observation::Absent);s.defaults=defaults;cases.push((s,Outcome::PreflightBlocked));}
        let mut recovery=ready(Observation::Absent);recovery.recovery=Recovery::Pending;cases.push((recovery,Outcome::PreflightBlocked));
        let mut fallback=ready(Observation::Absent);fallback.fallback=Fallback::Unavailable;cases.push((fallback,Outcome::PreflightBlocked));
        let mut app=ready(Observation::Absent);app.app_running=true;cases.push((app,Outcome::PreflightBlocked));
        for (after,expected) in cases {let mut host=Mock::new(ready(present()),after,LaunchResult::Exit(0));assert_eq!(run_removal(&mut host,true, |_|{}),expected);assert_eq!(host.events,["snapshot","prepare","snapshot","launch","snapshot"]);}
        let mut read_error=Mock::new(ready(present()),ready(Observation::Absent),LaunchResult::Exit(0)); read_error.snapshots[2]=Err(anyhow::anyhow!("post read failure"));assert_eq!(run_removal(&mut read_error,true, |_|{}),Outcome::UnknownAfter);
    }
    #[test] fn bilingual_outcomes_retranslate_and_no_history_required() {
        for code in 1..=14 {assert!(!outcome_message(code,true).is_empty());assert!(!outcome_message(code,false).is_empty());assert_ne!(outcome_message(code,true),outcome_message(code,false));}
        assert!(outcome_message(4,true).contains("Restart")); assert!(!outcome_message(1,true).contains("installed"));
        // Provenance is deliberately not part of authorization: explicit selection,
        // fresh identity/readiness, official completion and postchecks govern this flow.
        let mut host=Mock::new(ready(present()),ready(Observation::Absent),LaunchResult::Exit(0));
        assert_eq!(run_removal(&mut host,true, |_|{}),Outcome::ConfirmedAbsent);
    }
}

#[cfg(test)]
mod un04_restart_tests {
    use super::*;
    #[test] fn restart_notice_survives_app_failure_and_keep_or_absent_retry() {
        let warning=restart_recommended(false,Outcome::ConfirmedAbsent);assert!(warning);
        assert!(restart_recommended(warning,Outcome::Kept));assert!(restart_recommended(warning,Outcome::AlreadyAbsent));assert!(restart_recommended(false,Outcome::RestartRequired));
        assert!(!restart_recommended(false,Outcome::Cancelled));assert!(!restart_recommended(false,Outcome::Kept));
    }
}
#[cfg(test)]
mod output_removal_tests {
    use super::*;
    use crate::{cable_routing::tests::Mock, cable_evidence::{Observation,Recovery}, official_cable::LaunchResult};
    #[test] fn selected_click_routes_before_setup_success_retains_physical_and_volume_mute() {
        let mut host=Mock::new();let volumes=host.volumes.clone();let mut events=vec![];
        let result=run_removal_to_output(&mut host,true,"physical",|event|events.push(event));
        assert_eq!(result,(Outcome::ConfirmedAbsent,None));assert_eq!(host.ids,vec!["physical".to_string();3]);assert_eq!(host.launches,1);assert_eq!(host.volumes,volumes);
        assert_eq!(events,[Outcome::RoutingOutput,Outcome::PreparingOfficial,Outcome::AwaitingOfficial,Outcome::ConfirmedAbsent]);
    }
    #[test] fn keep_running_recovery_or_stale_selection_never_changes_routes() {
        let mut host=Mock::new();assert_eq!(run_removal_to_output(&mut host,false,"physical",|_|{}),(Outcome::Kept,None));assert!(host.writes.is_empty());
        for guard in 0..4 {
            let mut host=Mock::new();match guard {0=>host.running=true,1=>host.recovery=Recovery::Pending,2=>host.inventory.enumeration_failed=true,_=>()}
            let (outcome,detail)=run_removal_to_output(&mut host,true,if guard==3 {"stale"} else {"physical"},|_|{});
            assert_eq!(outcome,Outcome::PreflightBlocked);assert!(detail.is_some());assert!(host.writes.is_empty());assert_eq!(host.launches,0);
        }
    }
    #[test] fn routing_and_package_failure_restore_roles_and_expose_underlying_reason() {
        for route_failure in [false,true] {
            let mut host=Mock::new();host.after=None;host.prepare_ok=route_failure;
            if route_failure {host.fail_role=Some(1);}
            let (outcome,detail)=run_removal_to_output(&mut host,true,"physical",|_|{});
            assert_eq!(outcome,if route_failure {Outcome::PreflightBlocked} else {Outcome::PackageFailed});
            assert!(detail.unwrap().contains(if route_failure {"injected route error"} else {"injected download failure"}));
            assert_eq!(host.ids,vec!["cable".to_string();3]);assert_eq!(host.launches,0);
        }
    }
    #[test] fn cancel_and_known_official_failure_rollback_only_verified_present_cable() {
        for (launch,expected) in [(LaunchResult::Cancelled,Outcome::Cancelled),(LaunchResult::Failed,Outcome::LaunchFailed),(LaunchResult::Exit(5),Outcome::ExitFailed),(LaunchResult::Exit(0),Outcome::StillPresent)] {
            let mut host=Mock::new();host.launch=launch;host.after=None;
            assert_eq!(run_removal_to_output(&mut host,true,"physical",|_|{}).0,expected);assert_eq!(host.ids,vec!["cable".to_string();3]);
            let mut removed=Mock::new();removed.launch=launch;
            let result=run_removal_to_output(&mut removed,true,"physical",|_|{});
            assert_eq!(removed.ids,vec!["physical".to_string();3]);
            if launch != LaunchResult::Exit(0) {assert!(result.1.unwrap().contains("presence or identity"));}
        }
    }
    #[test] fn unknown_running_and_restart_results_keep_safe_physical_route() {
        for (launch,after,expected) in [(LaunchResult::Exit(0),Some(Observation::Unknown),Outcome::UnknownAfter),(LaunchResult::StillRunning,None,Outcome::SetupStillRunning),(LaunchResult::Exit(3010),None,Outcome::RestartRequired)] {
            let mut host=Mock::new();host.launch=launch;host.after=after;
            assert_eq!(run_removal_to_output(&mut host,true,"physical",|_|{}).0,expected);assert_eq!(host.ids,vec!["physical".to_string();3]);assert_eq!(host.writes.len(),3);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_identity_rejects_other_virtual_devices() {
        for name in [
            "Voicemeeter Input (VB-Audio Voicemeeter VAIO)",
            "Virtual Audio Cable",
            "CABLE Input",
            "VB-Audio Virtual Cable A",
            "Speakers",
            "Fake VB-Audio Virtual Cable device",
        ] {
            assert!(!is_vb_cable_interface(name), "{name}");
        }
        assert!(is_vb_cable_interface("VB-Audio Virtual Cable"));
        assert!(is_vb_cable_interface("vb-audio virtual cable"));
    }
    #[test]
    fn errors_are_unknown_unless_presence_is_proven() {
        assert_eq!(classify([]), Presence::Absent);
        assert_eq!(classify([Ok("Voicemeeter".into())]), Presence::Absent);
        assert_eq!(
            classify([Err(anyhow::anyhow!("property read failed"))]),
            Presence::Unknown
        );
        assert_eq!(
            classify([
                Err(anyhow::anyhow!("item failed")),
                Ok("VB-Audio Virtual Cable".into())
            ]),
            Presence::Present
        );
    }
    #[test]
    fn cable_defaults_to_keep_independently_of_data() {
        let defaults=UninstallOptions::default();
        assert!(!defaults.remove_cable && !defaults.delete_data);
        for delete_data in [false,true] {assert!(!UninstallOptions {delete_data,..defaults}.remove_cable);}
    }
}

