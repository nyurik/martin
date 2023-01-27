use crate::statics::{StaticsConfig, StaticsSourceEnum};
use crate::{Error, Result};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(clap::Args, Debug, PartialEq, Default)]
#[command(about, version)]
pub struct StaticsArgs {
    /// Make the content a file or a directory available as static files. Can be used multiple times.
    /// The name of the file or directory is used as the URL path, e.g. `-f foo` will make the
    /// content of the directory `foo` available as `/foo/*`.
    #[arg(short, long)]
    pub files: Option<Vec<PathBuf>>,
}

impl StaticsArgs {
    pub fn merge_into_config(self, config: &mut StaticsConfig) -> Result<()> {
        let files: Option<HashMap<String, StaticsSourceEnum>> = self.files.map(|v| {
            v.into_iter()
                .filter_map(|value| {
                    PathBuf::from(&value).file_stem().map(|v| {
                        let id = v.to_string_lossy().to_string();
                        (id, StaticsSourceEnum::Simple(value))
                    })
                })
                .collect()
        });

        *config = match (files, config.files.take()) {
            (Some(args), Some(mut cfg)) => {
                //merge two hashmaps, erroring out if there are duplicate keys
                for (k, v) in args {
                    match cfg.entry(k) {
                        Entry::Occupied(e) => {
                            if e.get() != &v {
                                return Err(Error::DuplicateSourceId(e.key().to_string()));
                            }
                        }
                        Entry::Vacant(e) => {
                            e.insert(v);
                        }
                    }
                }
                StaticsConfig { files: Some(cfg) }
            }
            (Some(files), None) => StaticsConfig { files: Some(files) },
            (None, files) => StaticsConfig { files },
        };

        Ok(())
    }
}
