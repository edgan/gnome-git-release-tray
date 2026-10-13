use crate::model::{Config, History};
use anyhow::{Context, Result, bail};
use directories::ProjectDirs;
use serde::{Serialize, de::DeserializeOwned};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
};

pub struct Store {
    pub config: Config,
    pub histories: BTreeMap<u64, History>,
    config_path: PathBuf,
    state_path: PathBuf,
}

impl Store {
    pub fn open() -> Result<Self> {
        let dirs = ProjectDirs::from("io", "github", "gnome-git-release-tray")
            .context("Cannot determine XDG directories")?;
        Self::at(
            dirs.config_dir().join("config.json"),
            dirs.state_dir()
                .context("No state directory")?
                .join("history.json"),
        )
    }
    pub fn at(config_path: PathBuf, state_path: PathBuf) -> Result<Self> {
        let config = read_config(&config_path)?;
        if !(1..=24).contains(&config.interval_hours) {
            bail!("Poll interval must be 1–24 hours");
        }
        let mut ids = std::collections::BTreeSet::new();
        for watch in &config.watches {
            if !ids.insert(watch.id) || watch.id >= config.next_id {
                bail!("Invalid watch IDs in configuration");
            }
            crate::model::repository(&format!("https://github.com/{}", watch.repository))?;
            fancy_regex::Regex::new(&watch.pattern)?;
        }
        Ok(Self {
            config,
            histories: read(&state_path)?,
            config_path,
            state_path,
        })
    }
    pub fn save_config(&self) -> Result<()> {
        atomic_write(&self.config_path, &self.config)
    }
    pub fn save_history(&self) -> Result<()> {
        atomic_write(&self.state_path, &self.histories)
    }
}

// Preserve existing durations where possible, rounding legacy minutes up to
// the next whole hour. New settings are written using interval_hours.
fn read_config(path: &Path) -> Result<Config> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(error) => return Err(error).with_context(|| format!("Cannot read {}", path.display())),
    };
    let mut value: serde_json::Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("Invalid data in {}", path.display()))?;
    if value.get("interval_hours").is_none()
        && let Some(minutes) = value.get("interval_minutes")
    {
        let minutes = minutes
            .as_u64()
            .context("Invalid legacy polling interval")?;
        if !(1..=1440).contains(&minutes) {
            bail!("Invalid legacy polling interval");
        }
        value["interval_hours"] = minutes.div_ceil(60).into();
    }
    serde_json::from_value(value).with_context(|| format!("Invalid data in {}", path.display()))
}

fn read<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("Invalid data in {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("Cannot read {}", path.display())),
    }
}

fn atomic_write(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("Missing parent directory")?;
    fs::create_dir_all(parent)?;
    let temp = path.with_extension("json.tmp");
    let result = (|| -> Result<()> {
        let mut file = File::create(&temp)?;
        file.write_all(&serde_json::to_vec_pretty(value)?)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    result.with_context(|| format!("Cannot save {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migrates_minutes_to_whole_hours() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.json");
        let state = dir.path().join("history.json");
        for (minutes, hours) in [(5, 1), (60, 1), (90, 2), (300, 5), (1440, 24)] {
            fs::write(
                &config,
                format!(r#"{{"interval_minutes":{minutes},"next_id":1,"watches":[]}}"#),
            )
            .unwrap();
            let store = Store::at(config.clone(), state.clone()).unwrap();
            assert_eq!(store.config.interval_hours, hours);
            store.save_config().unwrap();
            let saved = fs::read_to_string(&config).unwrap();
            assert!(saved.contains("interval_hours"));
            assert!(!saved.contains("interval_minutes"));
        }
    }

    #[test]
    fn round_trip_and_corruption() {
        let d = tempfile::tempdir().unwrap();
        let c = d.path().join("config.json");
        let s = d.path().join("state.json");
        let mut store = Store::at(c.clone(), s.clone()).unwrap();
        store.config.interval_hours = 10;
        store.histories.insert(
            1,
            History {
                initialized: true,
                ..Default::default()
            },
        );
        store.save_config().unwrap();
        store.save_history().unwrap();
        let restored = Store::at(c.clone(), s.clone()).unwrap();
        assert_eq!(restored.config.interval_hours, 10);
        assert!(restored.histories[&1].initialized);
        fs::write(&c, "broken").unwrap();
        assert!(Store::at(c, s).is_err());
    }
}
