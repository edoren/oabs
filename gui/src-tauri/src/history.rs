use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
};

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct MyHistory {
    history: VecDeque<String>,
    max_entries: Option<usize>,
    no_duplicates: bool,
}

impl MyHistory {
    pub fn new(max_entries: Option<usize>, no_duplicates: bool) -> Self {
        Self {
            history: VecDeque::new(),
            max_entries,
            no_duplicates,
        }
    }

    pub fn get_last(&self) -> Option<String> {
        self.history.front().cloned()
    }

    pub fn add(&mut self, val: String) {
        if self.no_duplicates {
            self.history.retain(|v| v != &val);
        }

        self.history.push_front(val);

        if let Some(max_entries) = self.max_entries {
            self.history.truncate(max_entries);
        }
    }
}

pub struct HistoryFile {
    history_file: PathBuf,
    history_map: HashMap<String, MyHistory>,
    max_entries: Option<usize>,
    no_duplicates: bool,
}

impl HistoryFile {
    pub fn new(
        history_file: &Path,
        max_entries: Option<usize>,
        no_duplicates: bool,
    ) -> Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(history_file)?;

        return Ok(Self {
            history_file: history_file.to_path_buf(),
            history_map: serde_json::from_reader(file).unwrap_or(HashMap::new()),
            max_entries,
            no_duplicates,
        });
    }

    pub fn get(&mut self, key: &str) -> &mut MyHistory {
        return self
            .history_map
            .entry(key.to_string())
            .or_insert_with(|| MyHistory::new(self.max_entries, self.no_duplicates));
    }
}

impl Drop for HistoryFile {
    fn drop(&mut self) {
        if let Ok(file) = std::fs::File::create(&self.history_file) {
            let _ = serde_json::to_writer(file, &self.history_map);
        }
    }
}
