use crate::error::Error;
use cargo_toml::Manifest;
use miette::{IntoDiagnostic, Result, WrapErr};
use serde::Deserialize;
use std::path::Path;

#[derive(Clone, Debug, Deserialize, Default)]
pub struct CargoEspFlashMeta {
    pub partition_table: Option<String>,
    pub bootloader: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Meta {
    pub espflash: Option<CargoEspFlashMeta>,
}

impl CargoEspFlashMeta {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<CargoEspFlashMeta> {
        let manifest = Manifest::<Meta>::from_path_with_metadata(path)
            .into_diagnostic()
            .wrap_err("Failed to parse Cargo.toml")?;
        let meta = manifest
            .package
            .and_then(|pkg| pkg.metadata)
            .unwrap_or_default()
            .espflash
            .unwrap_or_default();
        match meta.partition_table {
            Some(table) if !table.ends_with(".csv") => {
                return Err(Error::InvalidPartitionTablePath.into())
            }
            _ => {}
        }
        match meta.bootloader {
            Some(table) if !table.ends_with(".bin") => {
                return Err(Error::InvalidBootloaderPath.into())
            }
            _ => {}
        }
        Ok(meta)
    }
}
