use crate::source::is_valid_id_char;
use crate::statics::{StaticsConfig, StaticsSourceEnum};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(clap::Args, Debug, PartialEq, Default)]
#[command(about, version)]
pub struct StaticsArgs {
    /// Make the content a file or a directory available as static files. Can be used multiple times.
    /// The name of the file or directory is used as the URL path, e.g. `-f foo` will make the
    /// content of the directory `foo` available as `/foo/*`.
    #[arg(short, long)]
    pub files: Option<Vec<String>>,
}

impl StaticsArgs {
    pub fn merge_into_config(self, config: &mut Option<StaticsConfig>) {
        let files = self.files.map(|v| {
            v.into_iter()
                .filter_map(|value| {
                    if let Some((k, v)) = value.split_once(':') {
                        // if all characters in k are valid ID characters, treat it as a source ID
                        if !v.starts_with("//") && k.chars().all(is_valid_id_char) {
                            return Some((k.to_string(), StaticsSourceEnum::Simple(v.to_string())));
                        }
                    }
                    PathBuf::from(&value).file_stem().map(|v| {
                        let id = v.to_string_lossy().to_string();
                        (id, StaticsSourceEnum::Simple(value))
                    })
                })
                .collect()
        });

        config
            .files
            .filter(|v: &HashMap<String, StaticsSourceEnum>| !v.is_empty())
            .map(|files| StaticsConfig { files })
    }
}
