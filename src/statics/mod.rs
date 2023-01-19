use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct StaticsConfig {
    pub files: HashMap<String, StaticsSourceEnum>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum StaticsSourceEnum {
    Simple(String),
    Complex(StaticsSource),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StaticsSource {
    path: PathBuf,
    index_file: Option<PathBuf>,
}

impl StaticsConfig {
    // pub fn merge(&mut self, other: Self) -> &mut Self {
    //     self.files = match (mem::replace(&mut self.files, None), other.files) {
    //         (Some(mut first), Some(second)) => {
    //             // TODO: decide what to do if the key is the same. Also normalize slashes?
    //             first.extend(second);
    //             Some(first)
    //         }
    //         (None, Some(second)) => Some(second),
    //         (first, None) => first,
    //     };
    //     self
    // }
    //
    // pub fn finalize(self) -> StaticsConfig {
    //     // TODO
    //     self
    // }
}
