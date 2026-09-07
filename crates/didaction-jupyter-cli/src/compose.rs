use crate::config::{RuntimeKind, Settings};
use std::{fs, io, path::Path};

pub fn write(settings: &Settings, directory: &Path) -> io::Result<()> {
    let workspace = if settings.workspace.is_absolute() {
        settings.workspace.clone()
    } else {
        directory.join(&settings.workspace)
    };
    fs::create_dir_all(&workspace)?;
    let secrets = directory.join("secrets");
    fs::create_dir_all(&secrets)?;
    let token = secrets.join("jupyter-token");
    if !token.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "run `djupctl init` to generate the deployment secret",
        ));
    }
    let profile = settings
        .kernels
        .get(&settings.default_kernel)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "default kernel profile is missing",
            )
        })?;
    if !profile.enabled || profile.runtime != RuntimeKind::Server {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the default deployment kernel must be an enabled server profile",
        ));
    }
    let image = profile
        .image
        .as_deref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "server profile lacks image"))?;
    let compose = format!(
        "name: didaction\nservices:\n  kernel:\n    image: {image}\n    environment:\n      DIDACTION_NOTEBOOK_WORKSPACE: /workspace\n      DIDACTION_JUPYTER_TOKEN_FILE: /run/secrets/jupyter_token\n    volumes:\n      - {workspace}:/workspace\n    secrets: [jupyter_token]\n    security_opt: [no-new-privileges:true]\n    cap_drop: [ALL]\n  gateway:\n    image: ghcr.io/didaction/didaction-jupyter-fork-gateway:latest\n    depends_on: [kernel]\n    environment:\n      DIDACTION_JUPYTER_URL: http://kernel:8888\n      DIDACTION_JUPYTER_KERNEL: {kernelspec}\n      DIDACTION_JUPYTER_TOKEN_FILE: /run/secrets/jupyter_token\n      DIDACTION_NOTEBOOK_PATH: notebook.ipynb\n    ports: [\"127.0.0.1:{port}:8080\"]\n    secrets: [jupyter_token]\n    security_opt: [no-new-privileges:true]\n    cap_drop: [ALL]\nsecrets:\n  jupyter_token:\n    file: {directory}/secrets/jupyter-token\n",
        workspace = workspace.display(),
        kernelspec = profile.kernelspec,
        port = settings.port,
        directory = directory.display(),
    );
    fs::write(directory.join("compose.yaml"), compose)
}
