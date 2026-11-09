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

/// Native process coverage of the health gate only. The child is this libtest
/// executable, never the embedded application or the normal installer entry.
#[cfg(test)]
mod native_health_verification {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::{fs, os::windows::process::CommandExt, process::Stdio};

    const CHILD_TEST: &str = "update_entry::native_health_verification::synthetic_health_child";
    const MODES: [&str; 8] = ["valid", "early-exit", "exit-during-grace", "wrong-pid", "wrong-token", "wrong-version", "wrong-image", "malformed-json"];

    fn evidence_directory() -> Result<PathBuf> {
        let workspace = shared::canonical(Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).context("Missing workspace")?)?;
        let requested = PathBuf::from(std::env::var_os("MAXI_NATIVE_HEALTH_DIR").context("Set MAXI_NATIVE_HEALTH_DIR to an existing workspace evidence directory")?);
        operations::safe_root(&requested)?;
        let evidence = shared::canonical(&requested)?;
        ensure!(evidence.is_dir() && evidence.starts_with(&workspace) && evidence != workspace, "Health evidence must be strictly within this workspace");
        Ok(evidence)
    }

    struct OwnedChild(Child);
    impl OwnedChild {
        fn finish(&mut self, stage: &Path) -> Result<std::process::ExitStatus> {
            fs::write(stage.join("release-child"), b"release")?;
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                if let Some(status) = self.0.try_wait()? { return Ok(status); }
                ensure!(Instant::now() < deadline, "Owned health child did not exit after release");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            // This handle came only from the explicit fixture spawn below. No
            // process enumeration, image-name kill, or user application shutdown.
            if self.0.try_wait().ok().flatten().is_none() { let _ = self.0.kill(); }
            let _ = self.0.wait();
        }
    }

    fn wait_marker(path: &Path, child: &mut Child) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(8);
        while !path.try_exists()? {
            ensure!(child.try_wait()?.is_none(), "Health worker exited before its receipt; inspect its log");
            ensure!(Instant::now() < deadline, "Health worker did not publish its receipt");
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    #[test]
    #[ignore = "internal bounded subprocess worker; requires the parent's fresh workspace contract"]
    fn synthetic_health_child() -> Result<()> {
        let evidence = evidence_directory()?;
        let nonce = std::env::var("MAXI_HEALTH_FIXTURE_NONCE")?;
        let token = std::env::var("MAXI_HEALTH_FIXTURE_TOKEN")?;
        let mode = std::env::var("MAXI_HEALTH_FIXTURE_MODE")?;
        ensure!(shared::valid_token(&nonce) && shared::valid_token(&token) && MODES.contains(&mode.as_str()), "Invalid health child contract");
        let run = shared::canonical(&evidence.join(format!("native-health-{nonce}")))?;
        ensure!(run.parent() == Some(evidence.as_path()), "Child run escaped its evidence directory");
        let contract: serde_json::Value = shared::read_json(&run.join("contract.json"))?;
        ensure!(contract["nonce"].as_str() == Some(nonce.as_str()) && contract["test"].as_str() == Some(CHILD_TEST), "Health contract does not match this worker");
        let root = shared::canonical(&run.join("App"))?;
        let executable = shared::canonical(&std::env::current_exe()?)?;
        ensure!(shared::same(&executable, &root.join("MaxiSoundSet.exe")), "Worker is not the owned libtest image");
        let args: Vec<_> = std::env::args().collect();
        ensure!(args.iter().any(|arg| arg == CHILD_TEST) && args.iter().any(|arg| arg == "--exact") && args.iter().any(|arg| arg == "--ignored"), "Worker requires the exact ignored test selector");
        let stage = shared::canonical(&root.join(format!(".maxi-update-{token}")))?;
        ensure!(stage.parent() == Some(root.as_path()), "Health stage escaped fixture root");
        shared::write_json(&stage.join("worker-receipt.json"), &serde_json::json!({"pid":std::process::id(),"executable":executable,"mode":mode,"token":token,"synthetic":true}))?;
        // Handshake keeps the grace-exit scenario from racing parent setup.
        let deadline = Instant::now() + Duration::from_secs(12);
        while !stage.join("start-child").try_exists()? {
            ensure!(Instant::now() < deadline, "Parent did not start the health worker");
            std::thread::sleep(Duration::from_millis(5));
        }
        if mode == "early-exit" { return Ok(()); }
        let mut health = Health { token: token.clone(), pid: std::process::id(), executable: executable.clone(), version: operations::VERSION.into() };
        match mode.as_str() {
            "wrong-pid" => health.pid = health.pid.checked_add(1).context("PID overflow")?,
            "wrong-token" => health.token = if token == "0".repeat(32) { "1".repeat(32) } else { "0".repeat(32) },
            "wrong-version" => health.version = "1.0.3".into(),
            "wrong-image" => health.executable = root.join("other-image.bin"),
            _ => (),
        }
        if mode == "malformed-json" { fs::write(stage.join("health.json"), b"not a JSON health record")?; }
        else { shared::write_json(&stage.join("health.json"), &health)?; }
        if mode == "exit-during-grace" { std::thread::sleep(Duration::from_millis(500)); return Ok(()); }
        while !stage.join("release-child").try_exists()? {
            ensure!(Instant::now() < deadline, "Parent did not release the health worker");
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    fn run_case(run: &Path, nonce: &str, mode: &str) -> Result<serde_json::Value> {
        let root = shared::canonical(&run.join("App"))?;
        let token = shared::token()?;
        let stage = root.join(format!(".maxi-update-{token}"));
        fs::create_dir(&stage)?;
        let log = fs::OpenOptions::new().write(true).create_new(true).open(run.join(format!("{mode}.log")))?;
        let mut child = OwnedChild(Command::new(root.join("MaxiSoundSet.exe"))
            .args(["--exact", CHILD_TEST, "--ignored", "--nocapture", "--test-threads=1"])
            .env("MAXI_HEALTH_FIXTURE_NONCE", nonce).env("MAXI_HEALTH_FIXTURE_TOKEN", &token).env("MAXI_HEALTH_FIXTURE_MODE", mode)
            .current_dir(run).creation_flags(0x08000000) // CREATE_NO_WINDOW
            .stdin(Stdio::null()).stdout(Stdio::from(log.try_clone()?)).stderr(Stdio::from(log)).spawn()?);
        wait_marker(&stage.join("worker-receipt.json"), &mut child.0)?;
        let receipt: serde_json::Value = shared::read_json(&stage.join("worker-receipt.json"))?;
        let receipt_image = PathBuf::from(receipt["executable"].as_str().context("Receipt missing image")?);
        ensure!(receipt["pid"].as_u64() == Some(child.0.id() as u64) && shared::same(&receipt_image, &root.join("MaxiSoundSet.exe"))
            && receipt["mode"].as_str() == Some(mode) && receipt["token"].as_str() == Some(token.as_str()), "Worker receipt does not match the held child");
        let request = UpdateRequest { root: root.clone(), from_exe: root.join("MaxiSoundSet.exe"), version: operations::VERSION.into(), token, size: 1, sha256: "0".repeat(64) };
        fs::write(stage.join("start-child"), b"start")?;
        let started = Instant::now();
        let outcome = await_health(&mut child.0, &stage, &request, &root);
        let elapsed_ms = started.elapsed().as_millis();
        let alive_at_return = child.0.try_wait()?.is_none();
        let observed_error = outcome.as_ref().err().map(|error| format!("{error:#}"));
        let expected = match mode {
            "valid" => None,
            "early-exit" => Some("Updated application exited before startup was verified"),
            "exit-during-grace" => Some("Updated application exited during startup confirmation"),
            "malformed-json" => Some("expected ident at line 1 column 2"),
            _ => Some("Updated application health acknowledgement is invalid"),
        };
        let validation = (|| -> Result<()> {
            ensure!(observed_error.as_deref() == expected, "{mode}: unexpected health outcome: {observed_error:?}");
            if mode == "valid" { ensure!(elapsed_ms >= 2000 && alive_at_return, "Valid health did not prove the live two-second grace"); }
            if ["wrong-pid", "wrong-token", "wrong-version", "wrong-image", "malformed-json"].contains(&mode) {
                ensure!(alive_at_return, "Invalid health case exited instead of being rejected while alive");
            }
            if ["wrong-pid", "wrong-token", "wrong-version", "wrong-image"].contains(&mode) {
                let health: Health = shared::read_json(&stage.join("health.json"))?;
                ensure!((health.pid != child.0.id()) == (mode == "wrong-pid")
                    && (health.token != request.token) == (mode == "wrong-token")
                    && (health.version != request.version) == (mode == "wrong-version")
                    && (!shared::same(&health.executable, &root.join("MaxiSoundSet.exe"))) == (mode == "wrong-image"),
                    "{mode}: health record must differ in exactly the intended identity field");
            }
            if mode == "malformed-json" { ensure!(fs::read(stage.join("health.json"))? == b"not a JSON health record", "Malformed health bytes differ from the intended fixture"); }
            if mode == "exit-during-grace" {
                let health: Health = shared::read_json(&stage.join("health.json"))?;
                ensure!(health_matches(&health, child.0.id(), &root.join("MaxiSoundSet.exe"), &request), "Grace-exit worker did not publish matching health");
            }
            Ok(())
        })();
        let exit = child.finish(&stage)?;
        ensure!(exit.success(), "{mode}: worker test failed; inspect its log");
        let stored_health = if mode == "early-exit" || mode == "malformed-json" { None } else { Some(shared::read_json::<Health>(&stage.join("health.json"))?) };
        let record = serde_json::json!({"case":mode,"status":if validation.is_ok(){"passed"}else{"failed"},"worker":receipt,
            "elapsed_ms":elapsed_ms,"child_alive_at_await_return":alive_at_return,"observed_error":observed_error,
            "expected_version":request.version,"stored_health":stored_health,
            "exit_code":exit.code(),"validation_error":validation.as_ref().err().map(|error|format!("{error:#}"))});
        fs::write(run.join(format!("{mode}.json")), serde_json::to_vec_pretty(&record)?)?;
        validation?;
        Ok(record)
    }

    #[test]
    #[ignore = "requires MAXI_NATIVE_HEALTH_DIR; launches only private copies of this libtest executable"]
    fn native_subprocess_health_accepts_only_live_matching_child() -> Result<()> {
        ensure!(operations::VERSION == "1.0.4", "This acceptance fixture targets 1.0.4");
        let evidence = evidence_directory()?;
        let nonce = shared::token()?;
        let run = evidence.join(format!("native-health-{nonce}"));
        fs::create_dir(&run)?;
        fs::create_dir(run.join("App"))?;
        let source = shared::canonical(&std::env::current_exe()?)?;
        let copy = run.join("App/MaxiSoundSet.exe");
        fs::copy(&source, &copy)?;
        let digest = |path: &Path| -> Result<String> { Ok(format!("{:x}", Sha256::digest(fs::read(path)?))) };
        let source_sha256 = digest(&source)?;
        ensure!(digest(&copy)? == source_sha256, "Copied worker differs from the current libtest executable");
        fs::write(run.join("App/other-image.bin"), b"owned non-application image fixture")?;
        shared::write_json(&run.join("contract.json"), &serde_json::json!({"nonce":nonce,"test":CHILD_TEST}))?;
        let mut cases = Vec::new();
        let result = (|| -> Result<()> { for mode in MODES { cases.push(run_case(&run, &nonce, mode)?); } Ok(()) })();
        let report = serde_json::json!({"status":if result.is_ok(){"passed"}else{"failed"},"error":result.as_ref().err().map(|error|format!("{error:#}")),
            "scope":"Production await_health gate with bounded native libtest subprocesses; synthetic health only",
            "synthetic_health_child":true,"authentic_application_launched":false,"production_handoff_or_transaction_run":false,
            "source_test_executable":source,"copied_test_executable":copy,"source_and_copy_sha256":source_sha256,"cases":cases});
        fs::write(run.join("report.json"), serde_json::to_vec_pretty(&report)?)?;
        eprintln!("Native synthetic health evidence: {}", run.join("report.json").display());
        result
    }
}

/// Runs only the two reviewed abort paths of the production helper. The setup
/// subprocess is libtest; no normal installer/app entry or success path is used.
#[cfg(test)]
mod native_handoff_negatives {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::{collections::BTreeMap, fs, os::windows::{fs::OpenOptionsExt, process::CommandExt}, process::Stdio};

    const WORKER: &str = "update_entry::native_handoff_negatives::restricted_handoff_worker";
    const OLD_SHA256: &str = "c71f3d2573ad848dca778e38479293e39b15f5e294401ef255dfb8609b8da5e5";
    const CORRUPTION: &[u8] = b"Owned handoff fixture: deliberately not a PE executable";
    const MODES: [&str; 2] = ["unaccepted", "changed-after-ready"];
    type Tree = BTreeMap<String, serde_json::Value>;

    fn workspace() -> Result<PathBuf> {
        shared::canonical(Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).context("Missing workspace")?)
    }
    fn evidence() -> Result<PathBuf> {
        let requested = PathBuf::from(std::env::var_os("MAXI_NATIVE_HANDOFF_DIR").context("Set MAXI_NATIVE_HANDOFF_DIR to the workspace handoff evidence directory")?);
        operations::safe_root(&requested)?;
        let path = shared::canonical(&requested)?;
        ensure!(path.is_dir() && path.starts_with(workspace()?) && path != workspace()?, "Handoff evidence must be strictly within this workspace");
        Ok(path)
    }
    fn digest(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }
    fn tree(root: &Path, directory: &Path) -> Result<Tree> {
        shared::no_links(directory)?;
        let mut result = Tree::new();
        for entry in fs::read_dir(directory)? {
            let entry = entry?; let path = entry.path(); shared::no_links(&path)?;
            if entry.file_type()?.is_dir() { result.extend(tree(root, &path)?); }
            else {
                ensure!(entry.file_type()?.is_file(), "Unexpected fixture member");
                let bytes = fs::read(&path)?;
                result.insert(path.strip_prefix(root)?.to_string_lossy().into_owned(), serde_json::json!({"size":bytes.len(),"sha256":digest(&bytes)}));
            }
        }
        Ok(result)
    }
    fn metadata() -> Result<serde_json::Value> {
        let mut values = BTreeMap::new();
        for name in ["DisplayName", "DisplayVersion", "InstallLocation", "UninstallString", "DisplayIcon", "Publisher", "NoModify", "NoRepair", "EstimatedSize"] {
            values.insert(name, platform::text(platform::UNINSTALL_KEY, name)?);
        }
        Ok(serde_json::json!({"registration":values,"startup":platform::text(platform::RUN_KEY,"MaxiSoundSet")?,"raw_update_metadata":platform::UpdateMetadataSnapshot::capture()?.record()}))
    }
    fn expected_message(mode: &str) -> &'static str {
        if mode == "unaccepted" { "Application did not accept this helper; no exit was requested" }
        else { "Installer SHA-256 mismatch; previous application could not be restarted safely: Installer SHA-256 mismatch" }
    }
    struct OwnedChild(Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() { let _ = self.0.kill(); }
            let _ = self.0.wait(); // Only the explicitly spawned, held libtest child.
        }
    }
    fn wait_file(path: &Path, child: &mut Child) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(12);
        while !path.try_exists()? {
            ensure!(child.try_wait()?.is_none(), "Handoff worker exited before expected record: {}", path.display());
            ensure!(Instant::now() < deadline, "Handoff worker did not publish {}", path.display());
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    #[test]
    #[ignore = "internal subprocess worker; requires a fresh owned workspace contract"]
    fn restricted_handoff_worker() -> Result<()> {
        let nonce = std::env::var("MAXI_HANDOFF_NONCE")?;
        let token = std::env::var("MAXI_HANDOFF_TOKEN")?;
        let mode = std::env::var("MAXI_HANDOFF_MODE")?;
        ensure!(shared::valid_token(&nonce) && shared::valid_token(&token) && MODES.contains(&mode.as_str()), "Invalid restricted worker contract");
        let base = evidence()?.join(format!("handoff-{nonce}"));
        let case = shared::canonical(&base.join(&mode))?;
        ensure!(case.parent() == Some(shared::canonical(&base)?.as_path()), "Worker case escaped its owned run");
        let contract: serde_json::Value = shared::read_json(&case.join("contract.json"))?;
        ensure!(contract["nonce"].as_str() == Some(nonce.as_str()) && contract["token"].as_str() == Some(token.as_str())
            && contract["mode"].as_str() == Some(mode.as_str()) && contract["worker"].as_str() == Some(WORKER), "Worker contract differs");
        let args: Vec<_> = std::env::args().collect();
        ensure!(args.iter().any(|arg|arg == WORKER) && args.iter().any(|arg|arg == "--exact") && args.iter().any(|arg|arg == "--ignored"), "Worker requires exact ignored selector");
        let temp = shared::canonical(&case.join("Temp"))?;
        for name in ["TEMP", "TMP"] { ensure!(shared::same(Path::new(&std::env::var_os(name).context("Missing child temp environment")?), &temp), "Child temp escaped its workspace"); }
        ensure!(shared::same(&std::env::temp_dir(), &temp), "Native temp resolution escaped workspace");
        let helper = shared::helper_directory(&token)?;
        let setup = shared::canonical(&std::env::current_exe()?)?;
        ensure!(helper.parent() == Some(temp.as_path()) && shared::same(&setup, &helper.join("Setup.exe")), "Worker image is not the owned temporary Setup.exe");
        let root = shared::canonical(&case.join("App"))?;
        // TLS isolation is installed on this exact worker/test thread BEFORE all
        // platform registry operations and remains alive through run()+snapshot.
        let scope = platform::TestScope::new(&case, false)?;
        platform::register(&root, "1.0.3", true, 123)?;
        platform::test_set_startup("owned startup sentinel --preserve")?;
        let before = tree(&root, &root)?; let metadata_before = metadata()?;
        let setup_bytes = fs::read(&setup)?;
        let cli = vec![setup.display().to_string(), "--update-root".into(), root.display().to_string(), "--update-from-exe".into(), root.join("MaxiSoundSet.exe").display().to_string(),
            "--update-version".into(), operations::VERSION.into(), "--update-token".into(), token.clone(), "--update-size".into(), setup_bytes.len().to_string(), "--update-sha256".into(), digest(&setup_bytes)];
        let request = UpdateRequest::from_args(&cli)?.context("Explicit helper CLI was not recognized")?;
        shared::write_json(&case.join("worker-entry.json"), &serde_json::json!({"pid":std::process::id(),"executable":setup,"token":token,"mode":mode,"temp":temp,"emulated_registry":true,"before_files":before,"metadata_before":metadata_before}))?;
        let start_deadline = Instant::now() + Duration::from_secs(12);
        while !case.join("start-worker").try_exists()? {
            ensure!(Instant::now() < start_deadline, "Parent did not start restricted worker");
            std::thread::sleep(Duration::from_millis(5));
        }
        let started = Instant::now();
        let outcome = super::run(request);
        let elapsed_ms = started.elapsed().as_millis();
        ensure!(outcome.as_ref().err().is_some_and(|error|format!("{error:#}") == "Update was stopped; see the owned helper result record"), "Helper did not return its recorded-failure outcome");
        let result: ResultEnvelope = shared::read_json(&helper.join("result.json"))?;
        ensure!(result.token == token && result.result.status == UpdateStatus::Failed && result.result.version == operations::VERSION
            && result.result.backup_path.is_none() && result.result.message == expected_message(&mode), "Helper failed outside the intended abort branch");
        let after = tree(&root, &root)?;
        let mut expected = before.clone();
        if mode == "unaccepted" {
            ensure!(!helper.join("accepted.json").try_exists()? && elapsed_ms >= 15000, "Unaccepted helper did not exercise its real fifteen-second deadline");
        } else {
            let accepted: Health = shared::read_json(&helper.join("accepted.json"))?;
            ensure!(accepted.pid == std::process::id() && accepted.token == token && shared::same(&accepted.executable,&setup) && accepted.version == operations::VERSION, "Parent did not supply matching acceptance");
            expected.insert("MaxiSoundSet.exe".into(), serde_json::json!({"size":CORRUPTION.len(),"sha256":digest(CORRUPTION)}));
            ensure!(fs::read(root.join("MaxiSoundSet.exe"))? == CORRUPTION, "Corruption did not persist through both validations");
        }
        ensure!(after == expected && metadata()? == metadata_before, "Helper changed files/data/startup/registration beyond intentional fixture corruption");
        ensure!(!root.join(format!(".maxi-update-{token}")).exists(), "Helper reached the transaction stage");
        shared::write_json(&case.join("worker-complete.json"), &serde_json::json!({"pid":std::process::id(),"mode":mode,"elapsed_ms":elapsed_ms,"result":result,"before_files":before,"after_files":after,
            "metadata_before":metadata_before,"metadata_after":metadata()?,"transaction_stage_absent":true,"authentic_application_launched":false}))?;
        drop(scope);
        Ok(())
    }

    fn run_case(base: &Path, nonce: &str, mode: &str, source: &Path, old: &[u8]) -> Result<serde_json::Value> {
        let case = base.join(mode); fs::create_dir(&case)?;
        let root = case.join("App"); fs::create_dir(&root)?; fs::create_dir(root.join("Data"))?;
        for name in shared::OWNED_FILES { fs::write(root.join(name), format!("Owned old fixture: {name}"))?; }
        fs::write(root.join("MaxiSoundSet.exe"), old)?;
        fs::write(root.join("Data/settings.json"), b"{\"hotkeys\":[{\"enabled\":false,\"virtual_key\":121}],\"unknown\":\"preserve\"}\r\n")?;
        fs::write(root.join("Data/log.bin"), [0, 1, 255, 10])?; fs::write(root.join("unrelated.txt"), b"preserve unrelated fixture")?;
        shared::write_json(&root.join(shared::MARKER), &serde_json::json!({"identity":shared::IDENTITY,"root":root,"version":"1.0.3","files":shared::OWNED_FILES,
            "desktop":true,"startup":true,"integrated":true,"sha256":OLD_SHA256}))?;
        let before = tree(&root, &root)?;
        let token = shared::token()?;
        let temp = case.join("Temp"); fs::create_dir(&temp)?;
        let helper = temp.join(format!("MAXI-SOUNDSET-Update-{token}")); fs::create_dir(&helper)?;
        let setup = helper.join("Setup.exe"); fs::copy(source, &setup)?;
        ensure!(digest(&fs::read(source)?) == digest(&fs::read(&setup)?), "Copied setup differs from libtest source");
        shared::write_json(&case.join("contract.json"), &serde_json::json!({"nonce":nonce,"token":token,"mode":mode,"worker":WORKER}))?;
        let log = fs::OpenOptions::new().write(true).create_new(true).open(case.join("worker.log"))?;
        let mut child = OwnedChild(Command::new(&setup).args(["--exact", WORKER, "--ignored", "--nocapture", "--test-threads=1"])
            .env("MAXI_HANDOFF_NONCE",nonce).env("MAXI_HANDOFF_TOKEN",&token).env("MAXI_HANDOFF_MODE",mode).env("TEMP",&temp).env("TMP",&temp)
            .current_dir(&case).creation_flags(0x08000000).stdin(Stdio::null()).stdout(Stdio::from(log.try_clone()?)).stderr(Stdio::from(log)).spawn()?);
        wait_file(&case.join("worker-entry.json"), &mut child.0)?;
        let entry: serde_json::Value = shared::read_json(&case.join("worker-entry.json"))?;
        ensure!(entry["pid"].as_u64() == Some(child.0.id() as u64) && entry["token"].as_str() == Some(token.as_str()) && entry["mode"].as_str() == Some(mode)
            && shared::same(Path::new(entry["executable"].as_str().context("Missing worker image")?),&setup), "Worker entry does not match the held child");
        ensure!(serde_json::to_value(&before)? == entry["before_files"], "Worker baseline differs from parent snapshot");
        fs::write(case.join("start-worker"), b"start")?;
        wait_file(&helper.join("ready.json"), &mut child.0)?;
        let ready: Health = shared::read_json(&helper.join("ready.json"))?;
        ensure!(ready.pid == child.0.id() && ready.token == token && ready.version == operations::VERSION && shared::same(&ready.executable,&setup), "Real helper readiness identity differs");
        ensure!(!helper.join("accepted.json").try_exists()? && tree(&root,&root)? == before, "Fixture changed before acceptance decision");
        let corruption_lock = if mode == "changed-after-ready" {
            fs::write(root.join("MaxiSoundSet.exe"), CORRUPTION)?;
            // Allow identity/hash reads, deny repair/rename until worker is done.
            let lock = fs::OpenOptions::new().read(true).share_mode(1).open(root.join("MaxiSoundSet.exe"))?;
            shared::write_json(&helper.join("accepted.json"), &ready)?;
            Some(lock)
        } else { None };
        let deadline = Instant::now() + Duration::from_secs(22);
        let exit = loop {
            if let Some(status) = child.0.try_wait()? { break status; }
            ensure!(Instant::now() < deadline, "Restricted handoff worker exceeded its deadline");
            std::thread::sleep(Duration::from_millis(10));
        };
        ensure!(exit.success(), "Restricted worker failed; inspect worker.log");
        let complete: serde_json::Value = shared::read_json(&case.join("worker-complete.json"))?;
        let result: ResultEnvelope = shared::read_json(&helper.join("result.json"))?;
        ensure!(complete["pid"].as_u64() == Some(child.0.id() as u64) && complete["mode"].as_str() == Some(mode), "Completion does not match child");
        ensure!(result.token == token && result.result.status == UpdateStatus::Failed && result.result.message == expected_message(mode) && result.result.backup_path.is_none(), "Final envelope does not match intended abort");
        let mut expected = before;
        if mode == "changed-after-ready" { expected.insert("MaxiSoundSet.exe".into(), serde_json::json!({"size":CORRUPTION.len(),"sha256":digest(CORRUPTION)})); }
        ensure!(tree(&root,&root)? == expected && complete["metadata_before"] == complete["metadata_after"], "Final parent snapshot differs");
        drop(corruption_lock);
        let record = serde_json::json!({"case":mode,"status":"passed","worker_entry":entry,"worker_complete":complete,"exit_code":exit.code(),"setup_sha256":digest(&fs::read(&setup)?)});
        fs::write(case.join("evidence.json"),serde_json::to_vec_pretty(&record)?)?;
        Ok(record)
    }

    #[test]
    #[ignore = "requires workspace MAXI_NATIVE_HANDOFF_DIR; runs only two reviewed production helper abort paths"]
    fn native_helper_rejects_unaccepted_and_changed_identity_handoffs() -> Result<()> {
        ensure!(operations::VERSION == "1.0.4", "Handoff acceptance fixture targets 1.0.4");
        let old_path = workspace()?.join("releases/v1.0.3/MAXI-SOUNDSET-v1.0.3.exe");
        shared::no_links(&old_path)?;
        let old = fs::read(&old_path)?;
        ensure!(digest(&old) == OLD_SHA256 && shared::payload_version(&old)? == "1.0.3", "Archived old application is not authentic 1.0.3");
        ensure!(shared::payload_version(operations::APP)? == "1.0.4", "Embedded application version differs");
        let source = shared::canonical(&std::env::current_exe()?)?;
        let nonce = shared::token()?;
        let base = evidence()?.join(format!("handoff-{nonce}")); fs::create_dir(&base)?;
        let mut cases = Vec::new();
        let outcome = (|| -> Result<()> { for mode in MODES { cases.push(run_case(&base,&nonce,mode,&source,&old)?); } ensure!(fs::read(&old_path)? == old,"Archived source changed"); Ok(()) })();
        let report = serde_json::json!({"status":if outcome.is_ok(){"passed"}else{"failed"},"error":outcome.as_ref().err().map(|error|format!("{error:#}")),
            "scope":"Real helper run readiness/acceptance abort paths; copied libtest setup; emulated registry; no app launch or upgrade transaction",
            "old_source":old_path,"old_app_sha256":OLD_SHA256,"embedded_current_app_sha256":digest(operations::APP),"setup_test_executable":source,"cases":cases});
        fs::write(base.join("report.json"),serde_json::to_vec_pretty(&report)?)?;
        eprintln!("Restricted handoff negative evidence: {}",base.join("report.json").display());
        outcome
    }
}
