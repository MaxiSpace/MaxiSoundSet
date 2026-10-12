use crate::{cable_evidence, settings};
use anyhow::Result;
use serde::{Deserialize, Serialize};

use std::{
    fs,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct PackageState {
    pub latest: u32,
    pub confirmed_pack: u32,
    pub sha256: String,
}
pub enum Event {
    Metadata(PackageState),
    Progress(f32),
    Message(String),
    CheckFinished { installed: bool, outcome: CheckOutcome },
    Done(bool),
    Downloaded,
    Error(String),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckOutcome {
    NotInstalled,
    NewerAvailable,
    NoNewer,
    InstalledVersionUnknown,
}
fn check_outcome(state: &PackageState, is_installed: bool) -> CheckOutcome {
    if !is_installed {
        CheckOutcome::NotInstalled
    } else if state.confirmed_pack == 0 {
        CheckOutcome::InstalledVersionUnknown
    } else if state.latest > state.confirmed_pack {
        CheckOutcome::NewerAvailable
    } else {
        CheckOutcome::NoNewer
    }
}
pub struct Manager {
    rx: Receiver<Event>,
    tx: Sender<Event>,
    pub busy: bool,
    pub state: PackageState,
}
impl Manager {
    pub fn new(data: &Path) -> Self {
        let (tx, rx) = mpsc::channel();
        let state = fs::read(data.join("components.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            rx,
            tx,
            busy: false,
            state,
        }
    }
    pub fn start(&mut self, data: PathBuf, install: bool) {
        self.start_action(data, install, false);
    }
    pub fn start_action(&mut self, data: PathBuf, install: bool, force_download: bool) {
        if self.busy {
            return;
        }
        self.busy = true;
        let tx = self.tx.clone();
        let previous = self.state.clone();
        thread::spawn(move || {
            let result = (|| -> Result<Option<bool>> {
                let mut state = previous;
                if !install && !force_download { state.latest = latest_pack()?; }
                let _ = tx.send(Event::Metadata(state.clone()));
                log::info!("Official VB-CABLE latest package: {}", state.latest);
                if !install && !force_download {
                    settings::write_json(&data.join("components.json"), &state)?;
                    let found = installed()?;
                    let _ = tx.send(Event::CheckFinished {
                        installed: found,
                        outcome: check_outcome(&state, found),
                    });
                    return Ok(None);
                }

                let acquired = crate::official_cable::acquire_package(&data, force_download, |p| { let _ = tx.send(Event::Progress(p)); })?;
                let _ = tx.send(Event::Message(if acquired.reused { "Using the verified saved VB-CABLE package." } else { "The official VB-CABLE package was downloaded and verified." }.into()));
                let selected_pack = acquired.pack;
                if !acquired.reused { state.latest = selected_pack; }
                let installer = acquired.installer;
                let hash = acquired.hash;
                if force_download {
                    settings::write_json(&data.join("components.json"), &state)?;
                    let _ = tx.send(Event::Metadata(state));
                    let _ = tx.send(Event::Downloaded);
                    return Ok(None);
                }
                state.sha256 = hash.clone();
                let before = cable_evidence::observe_native();
                let prior = cable_evidence::read_record(&data);
                let mut evidence = cable_evidence::InstallRecord::pending_with_prior(before.clone(), hash, prior.as_ref());
                settings::write_json(&data.join(cable_evidence::RECORD_FILE), &evidence)?;
                let _ = tx.send(Event::Message("Windows will request administrator access for the official VB-CABLE installer. Installing a new driver may require a restart.".into()));
                log::info!(
                    "Launching official VB-CABLE installer: {}",
                    installer.display()
                );
                let exit_code = launch_installer(&installer)?;
                evidence.official_exit = Some(exit_code);
                let mut detected = false;
                for _ in 0..15 {
                    detected = installed()?;
                    if detected {
                        break;
                    }
                    thread::sleep(Duration::from_secs(1));
                }
                // A detected endpoint alone cannot prove that an existing driver was updated.
                evidence.after = cable_evidence::observe_native();
                settings::write_json(&data.join(cable_evidence::RECORD_FILE), &evidence)?;
                if detected && before == cable_evidence::Observation::Absent {
                    state.confirmed_pack = selected_pack;
                }
                settings::write_json(&data.join("components.json"), &state)?;
                let _ = tx.send(Event::Metadata(state));
                let _ = tx.send(Event::Message(if detected { "VB-CABLE is available. Select Full boost and your playback device." } else { "Installer finished; VB-CABLE is not available yet. Restart Windows, then refresh devices. Check Log if it remains absent." }.into()));
                Ok(Some(detected))
            })();
            match result {
                Ok(Some(found)) => { let _ = tx.send(Event::Done(found)); }
                Ok(None) => {}
                Err(e) => {
                    log::error!("Audio components: {e:#}");
                    let _ = tx.send(Event::Error(format!("{e:#}")));
                }
            }
        });
    }
    pub fn poll(&mut self) -> Vec<Event> {
        let events: Vec<_> = self.rx.try_iter().collect();
        for e in &events {
            match e {
                Event::Metadata(s) => self.state = s.clone(),
                Event::CheckFinished { .. } | Event::Done(_) | Event::Downloaded | Event::Error(_) => self.busy = false,
                _ => (),
            }
        }
        events
    }
}
pub fn installed() -> Result<bool> {
    let inventory = cable_evidence::native_active_inventory()?;
    if inventory.endpoints.iter().any(|e| e.active && cable_evidence::is_vb_cable_interface(&e.interface)) { return Ok(true); }
    anyhow::ensure!(!inventory.enumeration_failed, "VB-CABLE endpoint availability could not be determined");
    Ok(false)
}
pub fn latest_pack() -> Result<u32> { crate::official_cable::latest_pack() }
fn download_package(data: &Path, pack: u32, tx: &Sender<Event>) -> Result<(PathBuf, String)> {
    let _ = tx.send(Event::Message("Downloading the official VB-CABLE package over verified HTTPS…".into()));
    crate::official_cable::download_package(data, pack, |p| {let _ = tx.send(Event::Progress(p));})
}
fn launch_installer(path: &Path) -> Result<u32> {crate::official_cable::launch_installer(path)}
pub fn check_download(data: &Path) -> Result<String> {
    let pack = latest_pack()?;
    let (tx, _) = mpsc::channel();
    let (installer, hash) = download_package(data, pack, &tx)?;
    Ok(format!(
        "Official Pack{pack}; installer={}; SHA256={hash}; elevated installer was NOT launched",
        installer.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_classifies_only_confirmed_installed_packages() {
        let mut state = PackageState { latest: 46, confirmed_pack: 45, sha256: String::new() };
        assert_eq!(check_outcome(&state, true), CheckOutcome::NewerAvailable);
        state.confirmed_pack = 46;
        assert_eq!(check_outcome(&state, true), CheckOutcome::NoNewer);
        state.confirmed_pack = 0;
        assert_eq!(check_outcome(&state, true), CheckOutcome::InstalledVersionUnknown);
        state.confirmed_pack = 46;
        assert_eq!(check_outcome(&state, false), CheckOutcome::NotInstalled);
    }

    #[test]
    fn manager_consumes_terminal_check_and_error_events() {
        let mut manager = Manager::new(Path::new("components-r3-synthetic-absent"));
        manager.busy = true;
        let state = PackageState { latest: 46, confirmed_pack: 45, sha256: String::new() };
        manager.tx.send(Event::Metadata(state)).unwrap();
        manager.tx.send(Event::CheckFinished { installed: true, outcome: CheckOutcome::NewerAvailable }).unwrap();
        let events = manager.poll();
        assert!(matches!(events.as_slice(), [Event::Metadata(_), Event::CheckFinished { .. }]));
        assert_eq!(manager.state.latest, 46);
        assert!(!manager.busy, "a finished check must allow retry");
        manager.busy = true;
        manager.tx.send(Event::Error("synthetic timeout".into())).unwrap();
        assert!(matches!(manager.poll().as_slice(), [Event::Error(_)]));
        assert!(!manager.busy, "a timeout must allow retry");
    }
}
