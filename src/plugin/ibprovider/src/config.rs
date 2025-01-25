use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IbProviderConfig {
    pub prefix: Option<PathBuf>,
    pub engine_basename: String,
}

impl IbProviderConfig {
    pub fn new(config: Option<&str>) -> anyhow::Result<Self> {
        let config = toml::from_str(config.unwrap_or(""))?;
        Ok(config)
    }
}

impl Default for IbProviderConfig {
    fn default() -> Self {
        IbProviderConfig {
            prefix: None,
            engine_basename: "ibprovider-engine".to_owned(),
        }
    }
}
