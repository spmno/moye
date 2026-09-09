// 任务持久化：JSON 文件存储。
// Task persistence: JSON file storage.

use std::path::{Path, PathBuf};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use crate::scheduler::ScheduledTask;

/// 任务存储。
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TaskStore {
    path: PathBuf,
    tasks: Vec<ScheduledTask>,
}

impl TaskStore {
    pub fn load(path: &str) -> Result<Self> {
        let p = Path::new(path);
        let tasks = if p.exists() {
            let data = std::fs::read_to_string(p)?;
            serde_json::from_str(&data).unwrap_or_default()
        } else {
            Vec::new()
        };
        Ok(Self {
            path: p.to_path_buf(),
            tasks,
        })
    }

    pub fn tasks(&self) -> &[ScheduledTask] {
        &self.tasks
    }

    pub fn task_mut(&mut self, id: &str) -> Option<&mut ScheduledTask> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }

    pub fn add(&mut self, task: ScheduledTask) {
        self.tasks.push(task);
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let len = self.tasks.len();
        self.tasks.retain(|t| t.id != id);
        self.tasks.len() != len
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let json = serde_json::to_string_pretty(&self.tasks)?;
        std::fs::write(&self.path, json)?;
        Ok(())
    }
}
