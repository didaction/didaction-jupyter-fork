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
        "name: didaction\nservices:\n  jupyter:\n    image: {image}\n    environment:\n      DIDACTION_NOTEBOOK_WORKSPACE: /workspace\n      DIDACTION_JUPYTER_TOKEN_FILE: /run/secrets/jupyter_token\n    volumes:\n      - {workspace}:/workspace\n    secrets: [jupyter_token]\n    security_opt: [no-new-privileges:true]\n    cap_drop: [ALL]\n  gateway:\n    image: ghcr.io/didaction/didaction-jupyter-fork-gateway:latest\n    depends_on: [jupyter]\n    environment:\n      DIDACTION_JUPYTER_URL: http://jupyter:8888\n      DIDACTION_JUPYTER_KERNEL: {kernelspec}\n      DIDACTION_JUPYTER_TOKEN_FILE: /run/secrets/jupyter_token\n      DIDACTION_NOTEBOOK_PATH: notebook.ipynb\n    ports: [\"127.0.0.1:{port}:8080\"]\n    secrets: [jupyter_token]\n    security_opt: [no-new-privileges:true]\n    cap_drop: [ALL]\nsecrets:\n  jupyter_token:\n    file: {directory}/secrets/jupyter-token\n",
        workspace = workspace.display(),
        kernelspec = profile.kernelspec,
        port = settings.port,
        directory = directory.display(),
    );
    fs::write(directory.join("compose.yaml"), compose)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_stack_uses_the_jupyter_allowed_hostname() {
        let directory = std::env::temp_dir().join(format!("djupctl-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(directory.join("secrets")).unwrap();
        fs::write(directory.join("secrets/jupyter-token"), "test-token").unwrap();
        let mut settings = Settings {
            default_kernel: "verus".into(),
            ..Settings::default()
        };
        settings.kernels.get_mut("verus").unwrap().enabled = true;
        write(&settings, &directory).unwrap();
        let compose = fs::read_to_string(directory.join("compose.yaml")).unwrap();
        assert!(compose.contains("  jupyter:\n"));
        assert!(compose.contains("DIDACTION_JUPYTER_URL: http://jupyter:8888"));
        fs::remove_dir_all(directory).unwrap();
    }
}
