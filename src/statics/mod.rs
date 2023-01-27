use crate::config::{copy_unrecognized_config, Unrecognized};
use crate::utils::sorted_opt_map;
use crate::Error;
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct StaticsConfig {
    /// A map of source IDs to file paths or config objects
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(serialize_with = "sorted_opt_map")]
    pub files: Option<HashMap<String, StaticsSourceEnum>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum StaticsSourceEnum {
    Simple(PathBuf),
    Complex(StaticsSource),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StaticsSource {
    path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    index_file: Option<PathBuf>,
    #[serde(flatten)]
    pub unrecognized: HashMap<String, Value>,
}

impl StaticsConfig {
    pub fn finalize(&self) -> Result<Unrecognized, Error> {
        let mut res = Unrecognized::new();
        if let Some(ref fs) = self.files {
            for (k, v) in fs {
                if let StaticsSourceEnum::Complex(s) = v {
                    copy_unrecognized_config(&mut res, &format!("files.{k}."), &s.unrecognized);
                }
            }
        }

        Ok(res)
    }
}
