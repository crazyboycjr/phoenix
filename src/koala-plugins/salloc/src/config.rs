use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SallocConfig {
    pub prefix: PathBuf,
    pub engine_basename: String,
}

impl Default for SallocConfig {
    fn default() -> Self {
        SallocConfig {
            prefix: PathBuf::from("/tmp/koala"),
            engine_basename: String::from("salloc"),
        }
    }
}

impl SallocConfig {
    pub fn from_path<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config = toml::from_str(&content)?;
        Ok(config)
    }
}