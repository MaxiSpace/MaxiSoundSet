use anyhow::{Context, Result};
use serde::Deserialize;
use std::{
    cmp::Ordering,
    io::Read,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};

pub const RELEASES_URL: &str = "https://github.com/MaxiSpace/MaxiSoundSet/releases";
const RELEASES_API: &str =
    "https://api.github.com/repos/MaxiSpace/MaxiSoundSet/releases?per_page=20";

pub enum Event {
    Found {
        version: String,
        update_available: bool,
    },
    Error(String),
}

pub struct Manager {
    rx: Receiver<Event>,
    tx: Sender<Event>,
    pub busy: bool,
}

impl Manager {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            rx,
            tx,
            busy: false,
        }
    }

    pub fn start(&mut self, current: &'static str) {
        if self.busy {
            return;
        }
        self.busy = true;
        let tx = self.tx.clone();
        thread::spawn(move || match latest_release(current) {
            Ok((version, update_available)) => {
                let _ = tx.send(Event::Found {
                    version,
                    update_available,
                });
            }
            Err(error) => {
                log::error!("Application update check: {error:#}");
                let _ = tx.send(Event::Error(format!("{error:#}")));
            }
        });
    }

    pub fn poll(&mut self) -> Vec<Event> {
        let events: Vec<_> = self.rx.try_iter().collect();
        if !events.is_empty() {
            self.busy = false;
        }
        events
    }
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Version {
    core: [u64; 3],
    beta: Option<u64>,
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core
            .cmp(&other.core)
            .then_with(|| match (self.beta, other.beta) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(left), Some(right)) => left.cmp(&right),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn parse_version(value: &str) -> Option<Version> {
    let lower = value.to_ascii_lowercase();
    let numbers: Vec<u64> = regex::Regex::new(r"\d+")
        .ok()?
        .find_iter(&lower)
        .filter_map(|m| m.as_str().parse().ok())
        .collect();
    if numbers.is_empty() {
        return None;
    }

    if lower.contains("beta") {
        // Both public labels ("Beta 1.4") and Cargo tags
        // ("v1.0.0-beta.4") represent the same project version.
        if numbers.len() == 2 {
            return Some(Version {
                core: [numbers[0], 0, 0],
                beta: Some(numbers[1]),
            });
        }
        return Some(Version {
            core: [
                numbers[0],
                *numbers.get(1).unwrap_or(&0),
                *numbers.get(2).unwrap_or(&0),
            ],
            beta: numbers.last().copied(),
        });
    }

    Some(Version {
        core: [
            numbers[0],
            *numbers.get(1).unwrap_or(&0),
            *numbers.get(2).unwrap_or(&0),
        ],
        beta: None,
    })
}

fn latest_release(current: &str) -> Result<(String, bool)> {
    let current = parse_version(current).context("Current application version is invalid")?;
    let client = reqwest::blocking::Client::builder()
        .https_only(true)
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("MaxiSoundSet/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.url().scheme() == "https"
                && attempt
                    .url()
                    .host_str()
                    .is_some_and(|host| host == "api.github.com")
                && attempt.previous().len() < 3
            {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()?;
    let response = client
        .get(RELEASES_API)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()?
        .error_for_status()?;
    let mut bytes = Vec::new();
    response
        .take(1_000_001)
        .read_to_end(&mut bytes)
        .context("Could not read the GitHub release list")?;
    anyhow::ensure!(bytes.len() <= 1_000_000, "GitHub release list is too large");
    let releases: Vec<Release> = serde_json::from_slice(&bytes)?;
    let (version, parsed) = releases
        .into_iter()
        .filter(|release| !release.draft)
        .filter_map(|release| parse_version(&release.tag_name).map(|v| (release.tag_name, v)))
        .max_by_key(|(_, version)| *version)
        .context("No versioned GitHub release was found")?;
    Ok((
        version.trim_start_matches(['v', 'V']).to_string(),
        parsed > current,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_beta_label_matches_cargo_version() {
        assert_eq!(parse_version("Beta 1.4"), parse_version("v1.0.0-beta.4"));
    }

    #[test]
    fn newer_beta_and_stable_release_sort_after_current() {
        let current = parse_version("1.0.0-beta.4").unwrap();
        assert!(parse_version("Beta 1.5").unwrap() > current);
        assert!(parse_version("v1.0.0").unwrap() > current);
        assert!(parse_version("Beta 1.3").unwrap() < current);
    }
}
