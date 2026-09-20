//! Serialize system scans and persistence away from the UI thread.
use crate::{audio_engine::AudioEngine, conflicts, core_audio, settings::Settings};
use std::{path::PathBuf, sync::mpsc};

pub enum Job {
    Initialize(PathBuf),
    Devices,
    Conflicts,
    Save(Settings, PathBuf),
    Startup(bool, PathBuf),
    Exit(AudioEngine, Settings, PathBuf),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    fn directory(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("maxi-beta2-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }
    #[test]
    fn queued_saves_and_exit_keep_the_latest_settings() {
        let data = directory("save-order");
        let worker = Background::new();
        let mut settings = Settings::default();
        settings.target = 65;
        worker
            .submit(Job::Save(settings.clone(), data.clone()))
            .unwrap();
        settings.target = 100;
        worker
            .submit(Job::Save(settings.clone(), data.clone()))
            .unwrap();
        settings.target = 90;
        worker
            .submit(Job::Exit(AudioEngine::new(), settings, data.clone()))
            .unwrap();
        for _ in 0..2 {
            match worker.events.recv_timeout(Duration::from_secs(5)).unwrap() {
                Event::Saved(result) => result.unwrap(),
                _ => panic!("Save events must precede Exit"),
            }
        }
        match worker.events.recv_timeout(Duration::from_secs(5)).unwrap() {
            Event::Exited(_, result) => result.unwrap(),
            _ => panic!("Exit result missing"),
        }
        assert_eq!(Settings::load(&data).target, 90);
        std::fs::remove_dir_all(data).unwrap();
    }
    #[test]
    fn failed_recovery_does_not_enable_audio_or_enumerate_devices() {
        let data = directory("invalid-recovery");
        std::fs::write(data.join("recovery.json"), "invalid JSON").unwrap();
        let worker = Background::new();
        worker.submit(Job::Initialize(data.clone())).unwrap();
        match worker.events.recv_timeout(Duration::from_secs(5)).unwrap() {
            Event::Initialized(result) => assert!(result.is_err()),
            _ => panic!("Recovery must be the first startup result"),
        }
        assert!(worker
            .events
            .recv_timeout(Duration::from_millis(50))
            .is_err());
        assert!(data.join("recovery.json").exists());
        std::fs::remove_dir_all(data).unwrap();
    }
    #[test]
    fn exit_is_not_blocked_by_a_settings_write_failure() {
        let directory = directory("exit-save-failure");
        let invalid_data = directory.join("not-a-directory");
        std::fs::write(&invalid_data, "unchanged marker").unwrap();
        let worker = Background::new();
        worker
            .submit(Job::Exit(
                AudioEngine::new(),
                Settings::default(),
                invalid_data.clone(),
            ))
            .unwrap();
        match worker.events.recv_timeout(Duration::from_secs(5)).unwrap() {
            Event::Exited(_, result) => result.unwrap(),
            _ => panic!("Exit result missing after settings failure"),
        }
        assert_eq!(
            std::fs::read_to_string(invalid_data).unwrap(),
            "unchanged marker"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}
pub enum Event {
    Initialized(anyhow::Result<()>),
    Devices(anyhow::Result<Vec<core_audio::DeviceInfo>>),
    Conflicts(anyhow::Result<Vec<conflicts::AudioApp>>),
    Saved(anyhow::Result<()>),
    Startup(bool, anyhow::Result<()>),
    Exited(AudioEngine, anyhow::Result<()>),
}
pub struct Background {
    pub jobs: mpsc::Sender<Job>,
    pub events: mpsc::Receiver<Event>,
}
impl Background {
    pub fn submit(&self, job: Job) -> anyhow::Result<()> {
        self.jobs
            .send(job)
            .map_err(|_| anyhow::anyhow!("Background worker is unavailable"))
    }
    pub fn new() -> Self {
        let (jobs, requests) = mpsc::channel();
        let (results, events) = mpsc::channel();
        std::thread::spawn(move || {
            let mut applied_startup = None;
            while let Ok(job) = requests.recv() {
                let event = match job {
                    Job::Initialize(data) => {
                        let result = core_audio::recover(&data).map(|_| ());
                        let recovered = result.is_ok();
                        if results.send(Event::Initialized(result)).is_err() {
                            break;
                        }
                        if !recovered {
                            continue;
                        }
                        Event::Devices(core_audio::devices())
                    }
                    Job::Devices => Event::Devices(core_audio::devices()),
                    Job::Conflicts => Event::Conflicts(conflicts::scan()),
                    Job::Save(mut settings, data) => {
                        if let Some(enabled) = applied_startup {
                            settings.startup = enabled;
                        }
                        Event::Saved(settings.save(&data))
                    }
                    Job::Startup(enabled, exe) => {
                        let result = crate::startup::matches(&exe, enabled).and_then(|same| {
                            if same {
                                Ok(())
                            } else {
                                crate::startup::apply(enabled, &exe)
                            }
                        });
                        if result.is_ok() {
                            applied_startup = Some(enabled);
                        }
                        Event::Startup(enabled, result)
                    }
                    Job::Exit(mut engine, mut settings, data) => {
                        if let Some(enabled) = applied_startup {
                            settings.startup = enabled;
                        }
                        log::info!("Exit app: restoring audio and saving settings");
                        let result = engine.shutdown();
                        if result.is_ok() {
                            if let Err(e) = settings.save(&data) {
                                log::error!("Saving settings on exit: {e:#}");
                            }
                        }
                        if result.is_ok() {
                            log::info!("Exit app: audio restored and background process stopped");
                        }
                        log::logger().flush();
                        Event::Exited(engine, result)
                    }
                };
                if results.send(event).is_err() {
                    break;
                }
            }
        });
        Self { jobs, events }
    }
}
