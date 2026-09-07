use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, io, path::PathBuf};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct KernelProfile {
    pub display_name: String,
    pub runtime: RuntimeKind,
    pub enabled: bool,
    pub image: Option<String>,
    pub kernelspec: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
    Server,
    Browser,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Settings {
    pub default_kernel: String,
    pub workspace: PathBuf,
    pub port: u16,
    pub kernels: BTreeMap<String, KernelProfile>,
}

impl Default for Settings {
    fn default() -> Self {
        let mut kernels = BTreeMap::new();
        kernels.insert(
            "python".into(),
            KernelProfile {
                display_name: "Python 3 (server)".into(),
                runtime: RuntimeKind::Server,
                enabled: true,
                image: Some("quay.io/jupyter/minimal-notebook:python-3.12".into()),
                kernelspec: "python3".into(),
            },
        );
        kernels.insert(
            "tlaplus".into(),
            KernelProfile {
                display_name: "TLA+".into(),
                runtime: RuntimeKind::Server,
                enabled: false,
                image: Some("ghcr.io/didaction/didaction-tlaplus:latest".into()),
                kernelspec: "tlaplus_jupyter".into(),
            },
        );
        kernels.insert(
            "verus".into(),
            KernelProfile {
                display_name: "Verus".into(),
                runtime: RuntimeKind::Server,
                enabled: false,
                image: Some("ghcr.io/didaction/didaction-verus:latest".into()),
                kernelspec: "verus".into(),
            },
        );
        for (id, name, spec) in [
            ("pyodide-314", "Pyodide · Python 3.14", "python"),
            ("pyodide-027", "Pyodide · Python 3.12", "python"),
            ("xeus-python-019", "Xeus-Python · Python 3.13", "xpython"),
        ] {
            kernels.insert(
                id.into(),
                KernelProfile {
                    display_name: name.into(),
                    runtime: RuntimeKind::Browser,
                    enabled: id == "pyodide-314",
                    image: None,
                    kernelspec: spec.into(),
                },
            );
        }
        Self {
            default_kernel: "python".into(),
            workspace: PathBuf::from("notebooks"),
            port: 5173,
            kernels,
        }
    }
}

pub fn directory() -> io::Result<PathBuf> {
    let root = dirs::home_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory unavailable"))?;
    Ok(root.join(".config").join("didaction"))
}

pub fn load() -> io::Result<Settings> {
    let text = fs::read_to_string(directory()?.join("config.toml"))?;
    toml::from_str(&text).map_err(io::Error::other)
}

pub fn save(settings: &Settings) -> io::Result<PathBuf> {
    let directory = directory()?;
    fs::create_dir_all(&directory)?;
    let encoded = toml::to_string_pretty(settings).map_err(io::Error::other)?;
    fs::write(directory.join("config.toml"), encoded)?;
    fs::write(
        directory.join("kernel-profiles.json"),
        serde_json::to_vec_pretty(settings).map_err(io::Error::other)?,
    )?;
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_catalog_separates_server_and_browser_profiles() {
        let settings = Settings::default();
        assert_eq!(settings.kernels["verus"].runtime, RuntimeKind::Server);
        assert_eq!(
            settings.kernels["pyodide-314"].runtime,
            RuntimeKind::Browser
        );
        assert!(settings.kernels["python"].enabled);
    }
}
