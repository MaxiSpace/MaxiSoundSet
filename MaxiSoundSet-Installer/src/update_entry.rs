//! Restricted entry point of the freshly verified official setup executable.
use anyhow::{bail, ensure, Context, Result};
use crate::{operations, platform, running_app, update_handoff::{self as shared, Health, ResultEnvelope, UpdateResult, UpdateStatus}};
use std::{path::{Path, PathBuf}, process::{Child, Command}, time::{Duration, Instant}};

pub struct UpdateRequest { root: PathBuf, from_exe: PathBuf, version: String, token: String, size: u64, sha256: String }
impl UpdateRequest {
    pub fn from_args(args: &[String]) -> Result<Option<Self>> {
        if !args.iter().skip(1).any(|s| s.starts_with("--update-")) { return Ok(None); }
        ensure!(args.len() == 13, "Invalid update helper argument count");
        let mut values = std::collections::HashMap::new();
        for pair in args[1..].chunks_exact(2) {
            ensure!(["--update-root","--update-from-exe","--update-version","--update-token","--update-size","--update-sha256"].contains(&pair[0].as_str()) && values.insert(pair[0].as_str(),pair[1].as_str()).is_none(), "Unknown or duplicate update helper argument");
        }
        let get = |key| values.get(key).copied().context("Missing update helper argument");
        let request = Self { root: PathBuf::from(get("--update-root")?), from_exe: PathBuf::from(get("--update-from-exe")?), version: get("--update-version")?.into(), token: get("--update-token")?.into(), size: get("--update-size")?.parse()?, sha256: get("--update-sha256")?.into() };
        ensure!(shared::valid_token(&request.token), "Invalid update token"); shared::version(&request.version)?;
        ensure!(request.size > 0 && request.sha256.len() == 64 && request.sha256.bytes().all(|c| c.is_ascii_hexdigit()), "Invalid verified installer description");
        Ok(Some(request))
    }
    fn validate(&self, setup: &Path) -> Result<PathBuf> {
        ensure!(self.version == operations::VERSION, "Requested update version differs from this installer");
        let directory = shared::helper_directory(&self.token)?;
        ensure!(shared::same(setup,&directory.join("Setup.exe")), "Update setup is not the owned temporary helper");
        shared::verify_file(setup,self.size,&self.sha256)?;
        let registered = platform::text(platform::UNINSTALL_KEY,"InstallLocation")?.context("Missing registered installation")?;
        ensure!(shared::same(Path::new(&registered),&self.root), "Registered update root changed");
        let (root,manifest) = shared::validate_target(Path::new(&registered),&self.from_exe)?;
        shared::ensure_helper_outside_data(&directory,&root)?;
        ensure!(shared::version(&self.version)? > shared::version(&manifest.version)?, "Update is not newer than the installed version");
        operations::preflight_update(&root,setup,&self.version)?; Ok(root)
    }
}

fn publish(request: &UpdateRequest, result: UpdateResult) -> Result<()> {
    let directory = shared::helper_directory(&request.token)?;
    shared::ensure_helper_outside_data(&directory,&request.root)?;
    shared::write_json(&directory.join("result.json"), &ResultEnvelope { token: request.token.clone(), result })
}
fn launch(root: &Path, request: &UpdateRequest, health: Option<&Path>) -> Result<Child> {
    let mut command = Command::new(root.join("MaxiSoundSet.exe"));
    if let Some(path) = health { command.arg("--update-health").arg(path); }
    command.arg("--update-result").arg(shared::helper_directory(&request.token)?.join("result.json")).arg("--update-token").arg(&request.token);
    Ok(command.spawn()?)
}
fn health_matches(health: &Health, child_pid: u32, executable: &Path, request: &UpdateRequest) -> bool {
    health.token == request.token && health.pid == child_pid && health.version == request.version && shared::same(&health.executable,executable)
}
fn await_health(child: &mut Child, stage: &Path, request: &UpdateRequest, root: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30); let path = stage.join("health.json");
    loop {
        ensure!(child.try_wait()?.is_none(), "Updated application exited before startup was verified");
        if path.exists() {
            let health: Health = shared::read_json(&path)?;
            ensure!(health_matches(&health,child.id(),&root.join("MaxiSoundSet.exe"),request), "Updated application health acknowledgement is invalid");
            let grace = Instant::now() + Duration::from_secs(2);
            while Instant::now() < grace { ensure!(child.try_wait()?.is_none(), "Updated application exited during startup confirmation"); std::thread::sleep(Duration::from_millis(25)); }
            return Ok(());
        }
        ensure!(Instant::now() < deadline, "Updated application did not confirm startup within thirty seconds");
        std::thread::sleep(Duration::from_millis(25));
    }
}
fn backup_path(root: &Path, token: &str) -> Option<PathBuf> { let path = root.join(format!(".maxi-update-{token}")); path.exists().then_some(path) }
pub fn run(request: UpdateRequest) -> Result<()> {
    let _root_lease = platform::acquire_root_lease(&request.root).context("UPD_HANDOFF")?;
    let setup = shared::canonical(&std::env::current_exe()?)?;
    let mut closed_root = None;
    let outcome = (|| -> Result<UpdateResult> {
        let root = request.validate(&setup).context("UPD_HANDOFF")?;
        let helper_dir = shared::helper_directory(&request.token)?;
        shared::write_json(&helper_dir.join("ready.json"), &Health { token: request.token.clone(), pid: std::process::id(), executable: setup.clone(), version: request.version.clone() })?;
        // A late helper must never close the app after its handoff timed out.
        let acceptance_deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if helper_dir.join("accepted.json").exists() {
                let accepted: Health = shared::read_json(&helper_dir.join("accepted.json"))?;
                ensure!(accepted.token == request.token && accepted.pid == std::process::id() && shared::same(&accepted.executable,&setup) && accepted.version == request.version, "Invalid handoff acceptance"); break;
            }
            ensure!(Instant::now() < acceptance_deadline, "Application did not accept this helper; no exit was requested");
            std::thread::sleep(Duration::from_millis(10));
        }
        closed_root = Some(root.clone());
        running_app::close(&root).context("UPD_RECOVERY")?;
        // Re-read all identities and bytes after the cooperative shutdown.
        request.validate(&setup)?;
        ensure!(!running_app::is_running(&root)?, "Application reopened before update");
        ensure!(crate::cable_evidence::recovery(&root.join("Data")) == crate::cable_evidence::Recovery::Clear, "UPD_RECOVERY: audio recovery is not clear");
        let mut transaction = match operations::begin_update(&root,&setup,&request.version,&request.token) {
            Ok(transaction) => transaction,
            Err(error) => {
                return Err(error);
            }
        };
        let mut child = match launch(&root,&request,Some(&transaction.stage.join("health.json"))) {
            Ok(child) => Some(child), Err(error) => {
                return recover(&mut transaction,&request,None,error);
            }
        };
        match await_health(child.as_mut().unwrap(),&transaction.stage,&request,&root) {
            Ok(()) => {
                let stage = transaction.stage.clone();
                // Publish success only after verification. Cleanup errors retain a recoverable path.
                if let Err(error) = transaction.commit() { return Ok(UpdateResult { status: UpdateStatus::RecoveryRequired, version: request.version.clone(), message: format!("New version is running; backup cleanup failed: {error:#}"), backup_path: Some(stage) }); }
                Ok(UpdateResult { status: UpdateStatus::Updated, version: request.version.clone(), message: "Updated application version and startup were verified".into(), backup_path: None })
            }
            Err(error) => recover(&mut transaction,&request,child.as_mut(),error),
        }
    })();
    let result = match outcome {
        Ok(result) => result,
        Err(error) => {
            let backup = backup_path(&request.root,&request.token);
            let mut message = format!("{error:#}");
            if backup.is_none() {
                if let Some(root) = &closed_root {
                    let restart = (|| -> Result<()> {
                        request.validate(&setup)?;
                        ensure!(!running_app::is_running(root)?, "An application is already running");
                        ensure!(crate::cable_evidence::recovery(&root.join("Data")) == crate::cable_evidence::Recovery::Clear, "Audio recovery must be clear before relaunch");
                        launch(root,&request,None)?; Ok(())
                    })();
                    if let Err(relaunch) = restart { message.push_str(&format!("; previous application could not be restarted safely: {relaunch:#}")); }
                }
            }
            UpdateResult { status: if backup.is_some() { UpdateStatus::RecoveryRequired } else { UpdateStatus::Failed }, version: request.version.clone(), message, backup_path: backup }
        },
    };
    let failed = matches!(result.status,UpdateStatus::Failed | UpdateStatus::RecoveryRequired);
    publish(&request,result)?;
    if failed { bail!("Update was stopped; see the owned helper result record"); } Ok(())
}
fn recover(transaction: &mut operations::PendingUpgrade, request: &UpdateRequest, child: Option<&mut Child>, error: anyhow::Error) -> Result<UpdateResult> {
    if let Some(child) = child {
        if child.try_wait()?.is_none() {
            if let Err(close) = running_app::close(&transaction.root) { return Ok(UpdateResult { status: UpdateStatus::RecoveryRequired, version: request.version.clone(), message: format!("{error:#}; safe exit failed: {close:#}"), backup_path: Some(transaction.stage.clone()) }); }
            ensure!(child.try_wait()?.is_some(), "Updated process has not exited; backups retained");
        }
    }
    if let Err(rollback) = transaction.rollback() { return Ok(UpdateResult { status: UpdateStatus::RecoveryRequired, version: request.version.clone(), message: format!("{error:#}; {rollback:#}"), backup_path: Some(transaction.stage.clone()) }); }
    match launch(&transaction.root,request,None) {
        Ok(_) => Ok(UpdateResult { status: UpdateStatus::RolledBack, version: transaction.previous_version.clone(), message: format!("{error:#}; previous application restored and launched"), backup_path: None }),
        Err(relaunch) => Ok(UpdateResult { status: UpdateStatus::Failed, version: transaction.previous_version.clone(), message: format!("{error:#}; previous files restored but launch failed: {relaunch:#}"), backup_path: None }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn explicit_helper_cli_rejects_unknown_duplicate_and_missing_arguments() {
        assert!(UpdateRequest::from_args(&["setup".into()]).unwrap().is_none());
        for args in [vec!["setup","--update-root","C:/private"],vec!["setup","--update-token","bad"]] { assert!(UpdateRequest::from_args(&args.into_iter().map(String::from).collect::<Vec<_>>()).is_err()); }
        let mut args = vec!["setup","--update-root","C:/private","--update-from-exe","C:/private/MaxiSoundSet.exe","--update-version","1.0.3","--update-token","0123456789abcdef0123456789abcdef","--update-size","42","--update-sha256","0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"].into_iter().map(String::from).collect::<Vec<_>>();
        assert!(UpdateRequest::from_args(&args).unwrap().is_some()); args[11] = "--unknown".into(); assert!(UpdateRequest::from_args(&args).is_err()); args[11] = "--update-size".into(); assert!(UpdateRequest::from_args(&args).is_err());
    }
    #[test] fn readiness_requires_pid_version_token_and_exact_executable() {
        let root = std::env::temp_dir().join(format!("MAXI-Handoff-test-{}",shared::token().unwrap())); std::fs::create_dir(&root).unwrap(); let exe = root.join("MaxiSoundSet.exe"); std::fs::write(&exe,b"own fixture").unwrap();
        let request = UpdateRequest { root: root.clone(), from_exe: exe.clone(), version:"1.0.3".into(), token:shared::token().unwrap(), size:42, sha256:"0".repeat(64) };
        let mut health = Health { token:request.token.clone(),pid:77,executable:exe.clone(),version:request.version.clone() };
        assert!(health_matches(&health,77,&exe,&request)); health.pid=78; assert!(!health_matches(&health,77,&exe,&request)); health.pid=77; health.version="1.0.2".into(); assert!(!health_matches(&health,77,&exe,&request)); health.version="1.0.3".into(); health.token="0".repeat(32); assert!(!health_matches(&health,77,&exe,&request)); std::fs::remove_dir_all(root).unwrap();
    }
}
