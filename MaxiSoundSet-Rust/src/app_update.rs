//! Explicit metadata discovery and bounded installer acquisition; never executes it.
use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    cmp::Ordering,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering as AO},
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
pub const RELEASES_URL: &str = "https://github.com/MaxiSpace/MaxiSoundSet/releases";
pub fn release_notes_url(version: &str) -> Option<String> {
    stable_tag(&format!("v{version}"))?;
    Some(format!("{RELEASES_URL}/tag/v{version}"))
}
const API: &str = "https://api.github.com/repos/MaxiSpace/MaxiSoundSet/releases";
const MAX_METADATA: usize = 2_000_000;
const MAX_PAYLOAD: u64 = 256 * 1024 * 1024;
const OWNER: &str = "MAXISOUNDSET-UPDATE-ACQUISITION-v1";
static NONCE: AtomicU64 = AtomicU64::new(0);
pub enum Event {
    Found {
        version: String,
        update_available: bool,
    },
    Progress(f32),
    Downloaded(crate::update_handoff::VerifiedInstaller),
    Cancelled,
    Error(String),
}
struct Control {
    cancel: AtomicBool,
    closed: AtomicBool,
    delivery: Mutex<()>,
}
impl Control {
    fn new() -> Self {
        Self {
            cancel: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            delivery: Mutex::new(()),
        }
    }
    fn check(&self) -> Result<()> {
        if self.cancel.load(AO::Acquire) || self.closed.load(AO::Acquire) {
            bail!(Cancelled)
        }
        Ok(())
    }
}
#[derive(Debug)]
struct Cancelled;
impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Update acquisition cancelled")
    }
}
impl std::error::Error for Cancelled {}
struct Message {
    job: u64,
    event: Event,
    selected: Option<Candidate>,
}
pub struct Manager {
    rx: Receiver<Message>,
    tx: Sender<Message>,
    pub busy: bool,
    job: u64,
    control: Arc<Control>,
    selected: Option<Candidate>,
}
impl Manager {
    pub fn release_notes_url(&self) -> Option<String> {
        release_notes_url(&self.selected.as_ref()?.version)
    }
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            rx,
            tx,
            busy: false,
            job: 0,
            control: Arc::new(Control::new()),
            selected: None,
        }
    }
    fn begin(&mut self) -> (u64, Arc<Control>, Sender<Message>) {
        self.job = self.job.wrapping_add(1);
        self.control = Arc::new(Control::new());
        self.busy = true;
        (self.job, self.control.clone(), self.tx.clone())
    }
    pub fn start(&mut self, current: &'static str) {
        if self.busy {
            return;
        }
        self.selected = None;
        let (job, control, tx) = self.begin();
        thread::spawn(move || {
            let (event, selected) = match discover(current, &control) {
                Ok(c) => (
                    Event::Found {
                        version: c.version.clone(),
                        update_available: c.available,
                    },
                    Some(c),
                ),
                Err(e) if e.is::<Cancelled>() || control.cancel.load(AO::Acquire) => {
                    (Event::Cancelled, None)
                }
                Err(e) => (Event::Error(format!("{e:#}")), None),
            };
            publish(
                &tx,
                &control,
                Message {
                    job,
                    event,
                    selected,
                },
            );
        });
    }
    /// Only the explicit Install action calls this.
    pub fn download(&mut self) -> Result<()> {
        ensure!(
            !self.busy,
            "UPD_DOWNLOAD: An update operation is already running"
        );
        let c = self
            .selected
            .clone()
            .context("UPD_RELEASE: Check for an update first")?;
        ensure!(
            c.available,
            "UPD_RELEASE: No newer stable update is available"
        );
        let (job, control, tx) = self.begin();
        thread::spawn(move || {
            let progress = |v| {
                let _ = tx.send(Message {
                    job,
                    event: Event::Progress(v),
                    selected: None,
                });
            };
            let event = match download(&c, &control, progress) {
                Ok(i) => Event::Downloaded(i),
                Err(e) if e.is::<Cancelled>() => Event::Cancelled,
                Err(e) => Event::Error(format!("{e:#}")),
            };
            publish(
                &tx,
                &control,
                Message {
                    job,
                    event,
                    selected: None,
                },
            );
        });
        Ok(())
    }
    pub fn cancel(&mut self) {
        if self.busy {
            let _guard = self
                .control
                .delivery
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            self.control.cancel.store(true, AO::Release);
        }
    }
    pub fn poll(&mut self) -> Vec<Event> {
        let _guard = self
            .control
            .delivery
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut events = Vec::new();
        for mut m in self.rx.try_iter() {
            if m.job != self.job {
                discard(&m.event);
                continue;
            }
            if self.control.cancel.load(AO::Acquire) {
                match m.event {
                    Event::Progress(_) => continue,
                    Event::Found { .. } | Event::Downloaded(_) => {
                        m.event = cancel_event(m.event);
                        m.selected = None
                    }
                    _ => (),
                }
            }
            if !matches!(m.event, Event::Progress(_)) {
                self.busy = false;
            }
            if let Some(c) = m.selected {
                self.selected = Some(c)
            }
            events.push(m.event);
        }
        events
    }
}
impl Drop for Manager {
    fn drop(&mut self) {
        let _guard = self
            .control
            .delivery
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.control.closed.store(true, AO::Release);
        self.control.cancel.store(true, AO::Release);
        for m in self.rx.try_iter() {
            discard(&m.event);
        }
    }
}
fn discard(e: &Event) {
    if let Event::Downloaded(i) = e {
        if let Err(e) = cleanup_ready(&i.path) {
            log::error!("Update temp cleanup: {e:#}")
        }
    }
}
fn cancel_event(e: Event) -> Event {
    if let Event::Downloaded(i) = e {
        if let Err(e) = cleanup_ready(&i.path) {
            return Event::Error(format!(
                "UPD_DOWNLOAD: Could not clean cancelled update: {e:#}"
            ));
        }
    }
    Event::Cancelled
}
fn publish(tx: &Sender<Message>, c: &Control, mut m: Message) {
    let _guard = c.delivery.lock().unwrap_or_else(|e| e.into_inner());
    if c.closed.load(AO::Acquire) {
        discard(&m.event);
        return;
    }
    if c.cancel.load(AO::Acquire) && matches!(m.event, Event::Found { .. } | Event::Downloaded(_)) {
        m.event = cancel_event(m.event);
        m.selected = None
    }
    if let Err(e) = tx.send(m) {
        discard(&e.0.event)
    }
}
#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}
#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
    state: String,
    digest: Option<String>,
}
#[derive(Clone, Debug)]
struct Candidate {
    version: String,
    url: String,
    name: String,
    size: u64,
    sha256: String,
    available: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Version {
    core: [u64; 3],
    beta: Option<u64>,
}
impl Ord for Version {
    fn cmp(&self, o: &Self) -> Ordering {
        self.core
            .cmp(&o.core)
            .then_with(|| match (self.beta, o.beta) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => a.cmp(&b),
            })
    }
}
impl PartialOrd for Version {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
fn parse_version(value: &str) -> Option<Version> {
    let lower = value.to_ascii_lowercase();
    let n: Vec<u64> = regex::Regex::new(r"\d+")
        .ok()?
        .find_iter(&lower)
        .filter_map(|m| m.as_str().parse().ok())
        .collect();
    if n.is_empty() {
        return None;
    }
    if lower.contains("beta") {
        if n.len() == 2 {
            return Some(Version {
                core: [n[0], 0, 0],
                beta: Some(n[1]),
            });
        }
        return Some(Version {
            core: [n[0], *n.get(1).unwrap_or(&0), *n.get(2).unwrap_or(&0)],
            beta: n.last().copied(),
        });
    }
    Some(Version {
        core: [n[0], *n.get(1).unwrap_or(&0), *n.get(2).unwrap_or(&0)],
        beta: None,
    })
}
fn stable_tag(s: &str) -> Option<Version> {
    let parts: Vec<_> = s.strip_prefix('v')?.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let mut core = [0; 3];
    for (i, p) in parts.iter().enumerate() {
        if p.is_empty()
            || !p.bytes().all(|b| b.is_ascii_digit())
            || (p.len() > 1 && p.starts_with('0'))
        {
            return None;
        }
        core[i] = p.parse().ok()?;
    }
    Some(Version { core, beta: None })
}
fn newest_official_release(releases: Vec<Release>) -> Result<(Release, Version)> {
    let mut stable: Vec<_> = releases
        .into_iter()
        .filter(|r| !r.draft && !r.prerelease)
        .filter_map(|r| stable_tag(&r.tag_name).map(|v| (r, v)))
        .collect();
    let newest = stable
        .iter()
        .map(|(_, v)| *v)
        .max()
        .context("UPD_RELEASE: No stable official release found")?;
    ensure!(
        stable.iter().filter(|(_, v)| *v == newest).count() == 1,
        "UPD_RELEASE: Duplicate newest releases are ambiguous"
    );
    let index = stable.iter().position(|(_, v)| *v == newest).unwrap();
    Ok(stable.swap_remove(index))
}
fn candidate(releases: Vec<Release>, current: &str) -> Result<Candidate> {
    let current = parse_version(current).context("UPD_RELEASE: Invalid current version")?;
    let (r, v) = newest_official_release(releases)?;
    let version = r.tag_name.trim_start_matches('v').to_owned();
    let name = format!("MAXI-SOUNDSET-v{version}-Setup.exe");
    let url =
        format!("https://github.com/MaxiSpace/MaxiSoundSet/releases/download/v{version}/{name}");
    if v <= current {
        // No installation is offered, so an older release's missing installer
        // must not turn a successful version check into a download error.
        return Ok(Candidate { version, url, name, size: 0, sha256: String::new(), available: false });
    }
    let mut assets = r.assets.into_iter().filter(|a| a.name == name);
    let a = assets
        .next()
        .context("UPD_ASSET: Newest release has no supported x64 installer")?;
    ensure!(
        assets.next().is_none(),
        "UPD_ASSET: Duplicate installer assets are ambiguous"
    );
    ensure!(
        a.state == "uploaded",
        "UPD_ASSET: Installer asset is not uploaded"
    );
    ensure!(
        a.browser_download_url == url,
        "UPD_ASSET: Installer URL does not match the official repository/tag/name"
    );
    ensure!(
        (1..=MAX_PAYLOAD).contains(&a.size),
        "UPD_ASSET: Installer metadata size is invalid"
    );
    let digest = a.digest.context("UPD_ASSET: Installer digest missing")?;
    let sha = digest
        .strip_prefix("sha256:")
        .context("UPD_ASSET: Installer digest is not SHA-256")?;
    ensure!(
        sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()),
        "UPD_ASSET: Installer SHA-256 invalid"
    );
    Ok(Candidate {
        version,
        url,
        name,
        size: a.size,
        sha256: sha.to_ascii_lowercase(),
        available: v > current,
    })
}
fn safe_url(value: &str, metadata: bool, initial: &str) -> Result<reqwest::Url> {
    ensure!(
        value.starts_with("https://"),
        "UPD_NETWORK: Only HTTPS allowed"
    );
    let authority = value[8..].split('/').next().unwrap_or("");
    ensure!(
        !authority.contains(['@', ':']),
        "UPD_NETWORK: Credentials and ports forbidden"
    );
    let u = reqwest::Url::parse(value).context("UPD_NETWORK: Invalid URL")?;
    ensure!(
        u.username().is_empty()
            && u.password().is_none()
            && u.port().is_none()
            && u.fragment().is_none(),
        "UPD_NETWORK: Invalid URL"
    );
    if metadata {
        ensure!(
            u.host_str() == Some("api.github.com")
                && u.path() == "/repos/MaxiSpace/MaxiSoundSet/releases",
            "UPD_NETWORK: Invalid API URL"
        );
    } else {
        match u.host_str() {
            Some("github.com") => ensure!(
                value == initial,
                "UPD_NETWORK: GitHub redirect path changed"
            ),
            Some("release-assets.githubusercontent.com" | "objects.githubusercontent.com") => (),
            _ => bail!("UPD_NETWORK: Redirect host forbidden"),
        }
    }
    Ok(u)
}
fn client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("MaxiSoundSet/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("UPD_NETWORK: Could not initialize HTTPS client")
}
fn request(
    client: &reqwest::blocking::Client,
    initial: &str,
    metadata: bool,
    c: &Control,
) -> Result<reqwest::blocking::Response> {
    let mut u = safe_url(initial, metadata, initial)?;
    for redirects in 0..=4 {
        c.check()?;
        let response = client
            .get(u.clone())
            .header(
                "Accept",
                if metadata {
                    "application/vnd.github+json"
                } else {
                    "application/octet-stream"
                },
            )
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .context("UPD_NETWORK: HTTPS request failed")?;
        c.check()?;
        if response.status().is_redirection() {
            ensure!(redirects < 4, "UPD_NETWORK: Too many redirects");
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .context("UPD_NETWORK: Redirect missing Location")?
                .to_str()
                .context("UPD_NETWORK: Invalid redirect")?;
            let absolute = if location.starts_with("//") {
                format!("https:{location}")
            } else if location.contains("://") {
                location.to_owned()
            } else {
                u.join(location)
                    .context("UPD_NETWORK: Invalid relative redirect")?
                    .to_string()
            };
            u = safe_url(&absolute, metadata, initial)?;
            continue;
        }
        return response
            .error_for_status()
            .context("UPD_NETWORK: HTTP request rejected");
    }
    bail!("UPD_NETWORK: Too many redirects")
}
fn read_bounded(reader: &mut impl Read, limit: usize, c: &Control) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 16384];
    loop {
        c.check()?;
        let n = reader
            .read(&mut chunk)
            .context("UPD_NETWORK: Could not read metadata")?;
        c.check()?;
        if n == 0 {
            break;
        }
        ensure!(
            n <= limit.saturating_sub(bytes.len()),
            "UPD_RELEASE: Metadata exceeds bound"
        );
        bytes.extend_from_slice(&chunk[..n]);
    }
    Ok(bytes)
}
fn discover(current: &str, c: &Control) -> Result<Candidate> {
    let client = client()?;
    let mut all = Vec::new();
    let mut total = 0;
    for page in 1..=10 {
        let mut response = request(&client, &format!("{API}?per_page=100&page={page}"), true, c)?;
        if let Some(size) = response.content_length() {
            ensure!(
                size <= MAX_METADATA.saturating_sub(total) as u64,
                "UPD_RELEASE: Metadata exceeds bound"
            );
        }
        let bytes = read_bounded(&mut response, MAX_METADATA - total, c)?;
        total += bytes.len();
        let batch: Vec<Release> =
            serde_json::from_slice(&bytes).context("UPD_RELEASE: Invalid release metadata")?;
        let finished = batch.len() < 100;
        all.extend(batch);
        if finished {
            c.check()?;
            return candidate(all, current);
        }
    }
    bail!("UPD_RELEASE: Release pagination exceeds bound")
}
fn no_links(path: &Path) -> Result<()> {
    for p in path.ancestors() {
        let m = fs::symlink_metadata(p)?;
        ensure!(!m.file_type().is_symlink(), "Update temp symlink forbidden");
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            ensure!(
                m.file_attributes() & 0x400 == 0,
                "Update temp reparse point forbidden"
            );
        }
    }
    Ok(())
}
fn outside_data(root: &Path, data: &Path) -> Result<()> {
    #[cfg(windows)]
    let inside = {
        let root = root.to_string_lossy().replace('/', "\\").to_lowercase();
        let data = data.to_string_lossy().replace('/', "\\").to_lowercase();
        root == data || root.starts_with(&(data.trim_end_matches('\\').to_owned() + "\\"))
    };
    #[cfg(not(windows))]
    let inside = root.starts_with(data);
    ensure!(
        !inside,
        "UPD_DOWNLOAD: Update temp must be outside application Data"
    );
    Ok(())
}
fn temp_root() -> Result<PathBuf> {
    let root = fs::canonicalize(std::env::temp_dir())?;
    no_links(&root)?;
    let executable = fs::canonicalize(std::env::current_exe()?)?;
    let data = executable
        .parent()
        .context("Missing application parent")?
        .join("Data");
    let data = if data.try_exists()? {
        fs::canonicalize(data)?
    } else {
        data
    };
    outside_data(&root, &data)?;
    Ok(root)
}
struct TempPackage {
    dir: PathBuf,
    partial: PathBuf,
    ready: PathBuf,
    armed: bool,
}
impl TempPackage {
    fn new(name: &str) -> Result<(Self, File)> {
        let root = temp_root()?;
        for _ in 0..8 {
            let dir = root.join(format!(
                "MaxiSoundSet-update-{}-{}-{}",
                std::process::id(),
                SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
                NONCE.fetch_add(1, AO::Relaxed)
            ));
            match fs::create_dir(&dir) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
            let mut p = Self {
                partial: dir.join("download.partial"),
                ready: dir.join(name),
                dir,
                armed: true,
            };
            let result = (|| -> Result<File> {
                no_links(&p.dir)?;
                let mut owner = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(p.dir.join("owner.txt"))?;
                owner.write_all(OWNER.as_bytes())?;
                Ok(OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&p.partial)?)
            })();
            match result {
                Ok(f) => return Ok((p, f)),
                Err(e) => {
                    let cleanup = p.cleanup();
                    return Err(cleanup.err().unwrap_or(e));
                }
            }
        }
        bail!("Could not create unique update temp")
    }
    fn cleanup(&mut self) -> Result<()> {
        for p in [&self.partial, &self.ready, &self.dir.join("owner.txt")] {
            if p.try_exists()? {
                no_links(p)?;
                fs::remove_file(p)?;
            }
        }
        fs::remove_dir(&self.dir)?;
        self.armed = false;
        Ok(())
    }
}
impl Drop for TempPackage {
    fn drop(&mut self) {
        if self.armed {
            if let Err(e) = self.cleanup() {
                log::error!("Update partial cleanup: {e:#}")
            }
        }
    }
}
fn cleanup_ready(path: &Path) -> Result<()> {
    let dir = path.parent().context("Missing update parent")?;
    no_links(dir)?;
    ensure!(
        dir.parent() == Some(temp_root()?.as_path())
            && dir
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("MaxiSoundSet-update-")),
        "Refuse cleanup outside owned update temp"
    );
    no_links(&dir.join("owner.txt"))?;
    ensure!(
        fs::read_to_string(dir.join("owner.txt"))? == OWNER,
        "Invalid update owner marker"
    );
    no_links(path)?;
    fs::remove_file(path)?;
    fs::remove_file(dir.join("owner.txt"))?;
    fs::remove_dir(dir)?;
    Ok(())
}
/// The UI/handoff worker owns a delivered installer and calls this after copying it.
pub(crate) fn discard_download(installer: &crate::update_handoff::VerifiedInstaller) -> Result<()> {
    ensure!(
        stable_tag(&format!("v{}", installer.version)).is_some(),
        "UPD_DOWNLOAD: Invalid cleanup version"
    );
    ensure!(
        installer.path.file_name().is_some_and(
            |name| name == format!("MAXI-SOUNDSET-v{}-Setup.exe", installer.version).as_str()
        ),
        "UPD_DOWNLOAD: Invalid cleanup filename"
    );
    cleanup_ready(&installer.path).context("UPD_DOWNLOAD: Could not clean owned installer")
}
fn copy_payload(
    reader: &mut impl Read,
    file: &mut File,
    candidate: &Candidate,
    c: &Control,
    mut progress: impl FnMut(f32),
) -> Result<()> {
    let mut sha = Sha256::new();
    let mut size = 0u64;
    let mut chunk = [0u8; 65536];
    progress(0.);
    loop {
        c.check()?;
        let n = reader
            .read(&mut chunk)
            .context("UPD_NETWORK: Installer read failed")?;
        c.check()?;
        if n == 0 {
            break;
        }
        size = size
            .checked_add(n as u64)
            .context("UPD_SIZE: Size overflow")?;
        ensure!(
            size <= candidate.size && size <= MAX_PAYLOAD,
            "UPD_SIZE: Installer exceeds declared size"
        );
        file.write_all(&chunk[..n])
            .context("UPD_DOWNLOAD: Could not write partial installer")?;
        sha.update(&chunk[..n]);
        progress((size as f64 / candidate.size as f64).clamp(0., 1.) as f32);
    }
    ensure!(
        size == candidate.size,
        "UPD_SIZE: Installer download truncated"
    );
    ensure!(
        format!("{:x}", sha.finalize()) == candidate.sha256,
        "UPD_HASH: Installer SHA-256 mismatch"
    );
    c.check()?;
    file.flush()
        .context("UPD_DOWNLOAD: Could not flush installer")?;
    file.sync_all()
        .context("UPD_DOWNLOAD: Could not sync installer")?;
    ensure!(
        file.metadata()
            .context("UPD_DOWNLOAD: Could not inspect written installer")?
            .len()
            == candidate.size,
        "UPD_SIZE: Written installer size mismatch"
    );
    Ok(())
}
fn acquire_reader(
    reader: &mut impl Read,
    candidate: &Candidate,
    c: &Control,
    progress: impl FnMut(f32),
) -> Result<crate::update_handoff::VerifiedInstaller> {
    let (mut p, mut file) =
        TempPackage::new(&candidate.name).context("UPD_DOWNLOAD: Could not create owned temp")?;
    let result = copy_payload(reader, &mut file, candidate, c, progress);
    drop(file);
    let result = result.and_then(|_| {
        c.check()?;
        fs::rename(&p.partial, &p.ready).context("UPD_DOWNLOAD: Could not finalize installer")?;
        c.check()?;
        Ok(())
    });
    if let Err(e) = result {
        p.cleanup()
            .context("UPD_DOWNLOAD: Could not clean incomplete update")?;
        if c.cancel.load(AO::Acquire) {
            bail!(Cancelled)
        }
        return Err(e);
    }
    let i = crate::update_handoff::VerifiedInstaller {
        path: p.ready.clone(),
        version: candidate.version.clone(),
        size: candidate.size,
        sha256: candidate.sha256.clone(),
    };
    p.armed = false;
    Ok(i)
}
fn download(
    candidate: &Candidate,
    c: &Control,
    progress: impl FnMut(f32),
) -> Result<crate::update_handoff::VerifiedInstaller> {
    c.check()?;
    let client = client()?;
    let mut response = match request(&client, &candidate.url, false, c) {
        Ok(response) => response,
        Err(_) if c.cancel.load(AO::Acquire) => bail!(Cancelled),
        Err(error) => return Err(error),
    };
    if let Some(size) = response.content_length() {
        ensure!(
            size == candidate.size,
            "UPD_SIZE: HTTP payload size differs from metadata"
        );
    }
    acquire_reader(&mut response, candidate, c, progress)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    #[test]
    fn release_notes_use_only_validated_stable_versions_in_the_fixed_repository() {
        assert_eq!(release_notes_url("1.0.4").as_deref(), Some("https://github.com/MaxiSpace/MaxiSoundSet/releases/tag/v1.0.4"));
        for version in ["", "v1.0.4", "01.0.4", "1.0.4-beta.1", "1.0.4/../../Other", "1.0.4?redirect=evil", "https://evil.example", "18446744073709551616.0.0"] {
            assert!(release_notes_url(version).is_none(), "{version}");
        }
        assert!(Manager::new().release_notes_url().is_none());
    }
    #[test]
    fn redirected_temp_cannot_enter_data_and_boundary_is_component_exact() {
        let data = Path::new("C:/fixture/App/Data");
        assert!(outside_data(data, data).is_err());
        assert!(outside_data(Path::new("C:/fixture/App/Data/Temp"), data).is_err());
        assert!(outside_data(Path::new("C:/fixture/App/Database"), data).is_ok());
        #[cfg(windows)]
        assert!(outside_data(Path::new("c:/FIXTURE/app/DATA/tmp"), data).is_err());
    }
    fn release(version: &str) -> serde_json::Value {
        let name = format!("MAXI-SOUNDSET-{version}-Setup.exe");
        serde_json::json!({"tag_name":version,"draft":false,"prerelease":false,"assets":[{"name":name,"browser_download_url":format!("https://github.com/MaxiSpace/MaxiSoundSet/releases/download/{version}/{name}"),"state":"uploaded","size":3,"digest":format!("sha256:{:x}",Sha256::digest(b"abc"))}]})
    }
    fn select(values: Vec<serde_json::Value>, current: &str) -> Result<Candidate> {
        candidate(
            serde_json::from_value(serde_json::Value::Array(values))?,
            current,
        )
    }
    fn fixture() -> Candidate {
        select(vec![release("v1.0.4")], "1.0.3").unwrap()
    }
    fn own_dirs() -> Vec<PathBuf> {
        let prefix = format!("MaxiSoundSet-update-{}-", std::process::id());
        let mut paths: Vec<_> = fs::read_dir(std::env::temp_dir())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(&prefix)
            })
            .collect();
        paths.sort();
        paths
    }
    #[test]
    fn semantic_max_uses_only_canonical_stable_tags() {
        let mut values = vec![release("v1.10.0"), release("v1.2.0")];
        for bad in [
            "v99.0.0-beta.1",
            "v99.0",
            "V99.0.0",
            "v099.0.0",
            "v99.0.0.1",
            "nightly999",
        ] {
            values.push(release(bad));
        }
        let mut draft = release("v100.0.0");
        draft["draft"] = true.into();
        values.push(draft);
        assert_eq!(select(values, "1.0.3").unwrap().version, "1.10.0");
        assert!(!select(vec![release("v1.0.2")], "1.0.3").unwrap().available);
        assert!(
            select(vec![release("v1.0.0")], "Beta 1.4")
                .unwrap()
                .available
        );
    }
    #[test]
    fn missing_newest_asset_never_falls_back_to_old_release() {
        let mut newest = release("v1.0.5");
        newest["assets"] = serde_json::json!([]);
        assert!(format!(
            "{:#}",
            select(vec![release("v1.0.4"), newest], "1.0.3").unwrap_err()
        )
        .contains("UPD_ASSET"));
    }
    #[test]
    fn no_newer_release_needs_no_asset_and_cannot_download() {
        let mut old = release("v1.0.2");
        old["assets"] = serde_json::json!([]);
        let candidate = select(vec![old], "1.0.3").unwrap();
        assert_eq!(candidate.version, "1.0.2");
        assert!(!candidate.available);
        let mut manager = Manager::new();
        manager.selected = Some(candidate);
        assert!(manager.download().is_err());
        assert!(!manager.busy);
    }
    #[test]
    fn duplicate_release_and_asset_are_ambiguous() {
        assert!(select(vec![release("v1.0.4"), release("v1.0.4")], "1.0.3").is_err());
        let mut r = release("v1.0.4");
        let a = r["assets"][0].clone();
        r["assets"].as_array_mut().unwrap().push(a);
        assert!(select(vec![r], "1.0.3").is_err());
    }
    #[test]
    fn metadata_asset_validation_rejects_wrong_name_state_url_digest_and_size() {
        for(field,value)in [("name",serde_json::json!("MAXI-SOUNDSET-v1.0.4-Setup.zip")),("state",serde_json::json!("open")),("browser_download_url",serde_json::json!("https://github.com/Other/Repo/releases/download/v1.0.4/MAXI-SOUNDSET-v1.0.4-Setup.exe")),("digest",serde_json::json!("sha1:123")),("digest",serde_json::json!("sha256:gggg")),("digest",serde_json::Value::Null),("size",serde_json::json!(0)),("size",serde_json::json!(MAX_PAYLOAD+1))]{let mut r=release("v1.0.4");r["assets"][0][field]=value;assert!(select(vec![r],"1.0.3").is_err(),"{field}");}
    }
    #[test]
    fn url_policy_rejects_credentials_ports_http_offhost_and_path_changes() {
        let c = fixture();
        assert!(safe_url(&c.url, false, &c.url).is_ok());
        for url in [
            "http://release-assets.githubusercontent.com/a",
            "https://release-assets.githubusercontent.com:443/a",
            "https://user@objects.githubusercontent.com/a",
            "https://release-assets.githubusercontent.com.evil/a",
            "https://github.com/Other/Repo/a",
            "https://objects.githubusercontent.com/a#fragment",
        ] {
            assert!(safe_url(url, false, &c.url).is_err(), "{url}");
        }
        for url in [
            "https://release-assets.githubusercontent.com/a?token=fixture",
            "https://objects.githubusercontent.com/a",
        ] {
            assert!(safe_url(url, false, &c.url).is_ok());
        }
        assert!(safe_url(&format!("{API}?page=1"), true, API).is_ok());
        assert!(safe_url(
            "https://api.github.com/repos/Other/Repo/releases",
            true,
            API
        )
        .is_err());
    }
    struct ReadFailure {
        first: bool,
    }
    impl Read for ReadFailure {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.first {
                self.first = false;
                buf[..2].copy_from_slice(b"ab");
                Ok(2)
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "fixture HTTP body read failure",
                ))
            }
        }
    }
    #[test]
    fn bounded_metadata_rejects_read_failure_and_oversize() {
        let c = Control::new();
        assert!(format!(
            "{:#}",
            read_bounded(&mut ReadFailure { first: true }, 100, &c).unwrap_err()
        )
        .contains("UPD_NETWORK"));
        assert!(read_bounded(&mut Cursor::new(b"abcd"), 3, &c).is_err());
        assert_eq!(
            read_bounded(&mut Cursor::new(b"abc"), 3, &c).unwrap(),
            b"abc"
        );
    }
    #[test]
    fn truncation_oversize_and_wrong_hash_clean_owned_partial() {
        let before = own_dirs();
        let c = fixture();
        for (bytes, code) in [
            (b"ab".as_slice(), "UPD_SIZE"),
            (b"abcd".as_slice(), "UPD_SIZE"),
            (b"xyz".as_slice(), "UPD_HASH"),
        ] {
            let error =
                acquire_reader(&mut Cursor::new(bytes), &c, &Control::new(), |_| {}).unwrap_err();
            assert!(format!("{error:#}").contains(code));
            assert_eq!(own_dirs(), before);
        }
    }
    #[test]
    fn http_body_read_failure_cleans_partial_then_retry_verifies_exact_bytes() {
        let before = own_dirs();
        let c = fixture();
        let error = acquire_reader(
            &mut ReadFailure { first: true },
            &c,
            &Control::new(),
            |_| {},
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("UPD_NETWORK"));
        assert_eq!(own_dirs(), before);
        let mut progress = Vec::new();
        let installer = acquire_reader(&mut Cursor::new(b"abc"), &c, &Control::new(), |v| {
            progress.push(v)
        })
        .unwrap();
        assert_eq!(fs::read(&installer.path).unwrap(), b"abc");
        assert_eq!(installer.size, 3);
        assert_eq!(installer.sha256, format!("{:x}", Sha256::digest(b"abc")));
        assert!(progress.iter().all(|v| *v >= 0. && *v <= 1.));
        assert_eq!(progress.first(), Some(&0.));
        assert_eq!(progress.last(), Some(&1.));
        discard_download(&installer).unwrap();
        assert_eq!(own_dirs(), before);
    }
    #[test]
    fn cancellation_after_progress_cleans_partial_and_never_ready() {
        let before = own_dirs();
        let control = Control::new();
        let error = acquire_reader(&mut Cursor::new(b"abc"), &fixture(), &control, |p| {
            if p > 0. {
                control.cancel.store(true, AO::Release)
            }
        })
        .unwrap_err();
        assert!(error.is::<Cancelled>());
        assert_eq!(own_dirs(), before);
        let control = Control::new();
        control.cancel.store(true, AO::Release);
        assert!(
            acquire_reader(&mut Cursor::new(b"abc"), &fixture(), &control, |_| {})
                .unwrap_err()
                .is::<Cancelled>()
        );
        assert_eq!(own_dirs(), before);
    }
    #[test]
    fn progress_is_nonterminal_and_download_requires_explicit_validated_selection() {
        let mut manager = Manager::new();
        assert!(manager.download().is_err());
        assert!(!manager.busy);
        let (job, _, tx) = manager.begin();
        assert!(manager.download().is_err());
        tx.send(Message {
            job,
            event: Event::Progress(0.5),
            selected: None,
        })
        .unwrap();
        assert!(matches!(manager.poll().as_slice(), [Event::Progress(_)]));
        assert!(manager.busy);
        tx.send(Message {
            job,
            event: Event::Found {
                version: "1.0.4".into(),
                update_available: true,
            },
            selected: Some(fixture()),
        })
        .unwrap();
        assert!(matches!(manager.poll().as_slice(), [Event::Found { .. }]));
        assert!(!manager.busy);
        assert!(manager.selected.is_some());
    }
    #[test]
    fn queued_ready_cancel_race_returns_cancelled_only_after_cleanup() {
        let before = own_dirs();
        let installer = acquire_reader(
            &mut Cursor::new(b"abc"),
            &fixture(),
            &Control::new(),
            |_| {},
        )
        .unwrap();
        let path = installer.path.clone();
        let mut m = Manager::new();
        let (job, c, tx) = m.begin();
        publish(
            &tx,
            &c,
            Message {
                job,
                event: Event::Downloaded(installer),
                selected: None,
            },
        );
        m.cancel();
        assert!(matches!(m.poll().as_slice(), [Event::Cancelled]));
        assert!(!m.busy);
        assert!(!path.exists());
        assert_eq!(own_dirs(), before);
    }
    #[test]
    fn stale_ready_cleans_owned_file_without_clearing_current_busy() {
        let before = own_dirs();
        let installer = acquire_reader(
            &mut Cursor::new(b"abc"),
            &fixture(),
            &Control::new(),
            |_| {},
        )
        .unwrap();
        let mut m = Manager::new();
        let (old, _, tx) = m.begin();
        let (current, _, _) = m.begin();
        tx.send(Message {
            job: old,
            event: Event::Downloaded(installer),
            selected: None,
        })
        .unwrap();
        tx.send(Message {
            job: current,
            event: Event::Progress(0.2),
            selected: None,
        })
        .unwrap();
        assert!(matches!(m.poll().as_slice(), [Event::Progress(_)]));
        assert!(m.busy);
        assert_eq!(own_dirs(), before);
        tx.send(Message {
            job: current,
            event: Event::Error("UPD_NETWORK: fixture".into()),
            selected: None,
        })
        .unwrap();
        assert!(matches!(m.poll().as_slice(), [Event::Error(_)]));
        assert!(!m.busy);
    }
    #[test]
    fn dropped_manager_and_closed_receiver_cleanup_ready() {
        let before = own_dirs();
        let installer = acquire_reader(
            &mut Cursor::new(b"abc"),
            &fixture(),
            &Control::new(),
            |_| {},
        )
        .unwrap();
        let mut m = Manager::new();
        let (job, c, tx) = m.begin();
        publish(
            &tx,
            &c,
            Message {
                job,
                event: Event::Downloaded(installer),
                selected: None,
            },
        );
        drop(m);
        assert_eq!(own_dirs(), before);
        let installer = acquire_reader(
            &mut Cursor::new(b"abc"),
            &fixture(),
            &Control::new(),
            |_| {},
        )
        .unwrap();
        publish(
            &tx,
            &c,
            Message {
                job,
                event: Event::Downloaded(installer),
                selected: None,
            },
        );
        assert_eq!(own_dirs(), before);
    }
    #[test]
    fn public_beta_label_matches_cargo_version() {
        assert_eq!(parse_version("Beta 1.4"), parse_version("v1.0.0-beta.4"));
    }
    #[test]
    fn newer_beta_and_stable_release_sort_after_current() {
        let c = parse_version("1.0.0-beta.4").unwrap();
        assert!(parse_version("Beta 1.5").unwrap() > c);
        assert!(parse_version("v1.0.0").unwrap() > c);
        assert!(parse_version("Beta 1.3").unwrap() < c);
    }
    #[test]
    fn update_check_ignores_drafts_and_prereleases() {
        let r:Vec<Release>=serde_json::from_str(r#"[{"tag_name":"v1.2.0-beta.1","draft":false,"prerelease":true},{"tag_name":"v1.1.1","draft":true,"prerelease":false},{"tag_name":"v1.1.0","draft":false,"prerelease":false}]"#).unwrap();
        assert_eq!(newest_official_release(r).unwrap().0.tag_name, "v1.1.0");
    }
}
