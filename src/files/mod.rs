use crate::source::is_valid_id_char;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::mem;
use std::path::PathBuf;

#[derive(clap::Args, Debug)]
#[command(about, version)]
pub struct StaticsArgs {
    /// Make the content a file or a directory available as static files. Can be used multiple times.
    /// The name of the file or directory is used as the URL path, e.g. `-f foo` will make the
    /// content of the directory `foo` available as `/foo/*`.
    #[arg(short, long)]
    pub files: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct FilesConfig {
    files: Option<HashMap<String, StaticsSourceEnum>>,
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

impl FilesConfig {
    pub fn merge(&mut self, other: Self) -> &mut Self {
        self.files = match (mem::replace(&mut self.files, None), other.files) {
            (Some(mut first), Some(second)) => {
                // TODO: decide what to do if the key is the same. Also normalize slashes?
                first.extend(second);
                Some(first)
            }
            (None, Some(second)) => Some(second),
            (first, None) => first,
        };
        self
    }

    pub fn finalize(self) -> FilesConfig {
        // TODO
        self
    }
}

impl From<StaticsArgs> for FilesConfig {
    fn from(args: StaticsArgs) -> Self {
        FilesConfig {
            files: args.files.map(|v| {
                v.into_iter()
                    .filter_map(|value| {
                        if let Some((k, v)) = value.split_once(':') {
                            // if all characters in k are valid ID characters, treat it as a source ID
                            if !v.starts_with("//") && k.chars().all(is_valid_id_char) {
                                return Some((
                                    k.to_string(),
                                    StaticsSourceEnum::Simple(v.to_string()),
                                ));
                            }
                        }
                        PathBuf::from(&value).file_stem().map(|v| {
                            let id = v.to_string_lossy().to_string();
                            (id, StaticsSourceEnum::Simple(value))
                        })
                    })
                    .collect()
            }),
        }
    }
}
