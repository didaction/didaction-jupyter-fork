use crate::config::{RuntimeKind, Settings};
use serde_json::json;
use std::{collections::BTreeMap, fs, io, path::Path};

pub fn write(settings: &Settings, directory: &Path) -> io::Result<()> {
    let workspace = if settings.workspace.is_absolute() {
        settings.workspace.clone()
    } else {
        directory.join(&settings.workspace)
    };
    fs::create_dir_all(&workspace)?;
    let startup_notebook = workspace.join("notebook.ipynb");
    if !startup_notebook.exists() {
        fs::write(
            startup_notebook,
            r#"{"cells":[],"metadata":{},"nbformat":4,"nbformat_minor":5}"#,
        )?;
    }
    let secrets = directory.join("secrets");
    fs::create_dir_all(&secrets)?;
    let token = secrets.join("jupyter-token");
    if !token.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "run `djupctl init` to generate the deployment secret",
        ));
    }
    let default = settings
        .kernels
        .get(&settings.default_kernel)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "default kernel profile is missing",
            )
        })?;
    if !default.enabled || default.runtime != RuntimeKind::Server {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the default deployment kernel must be an enabled server profile",
        ));
    }
    let mut services = String::new();
    let mut routes = BTreeMap::new();
    let mut dependencies = Vec::new();
    for (id, profile) in settings
        .kernels
        .iter()
        .filter(|(_, profile)| profile.enabled && profile.runtime == RuntimeKind::Server)
    {
        if !valid_id(id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "kernel profile id must contain only ASCII letters, digits, '-' or '_'",
            ));
        }
        let image = profile.image.as_deref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "server profile lacks image")
        })?;
        let service = format!("jupyter-{id}");
        dependencies.push(service.clone());
        routes.insert(
            id,
            json!({"url":format!("http://{service}:8888"),"kernelspec":profile.kernelspec}),
        );
        services.push_str(&format!(
            "  {service}:\n    image: {image}\n    hostname: {service}\n{command}    environment:\n      DIDACTION_NOTEBOOK_WORKSPACE: /workspace\n      DIDACTION_JUPYTER_TOKEN_FILE: /run/secrets/jupyter_token\n    volumes:\n      - {workspace}:/workspace\n    secrets: [jupyter_token]\n    security_opt: [no-new-privileges:true]\n    cap_drop: [ALL]\n",
            workspace = workspace.display(),
            command = if image.starts_with("quay.io/jupyter/") {
                "    command:\n      - bash\n      - -lc\n      - 'exec start-notebook.py --ServerApp.token=\"$$(cat /run/secrets/jupyter_token)\" --ServerApp.root_dir=/workspace --ServerApp.allow_remote_access=True'\n"
            } else { "" },
        ));
    }
    if routes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "enable at least one server kernel profile",
        ));
    }
    fs::write(
        directory.join("kernel-profiles.json"),
        serde_json::to_vec_pretty(&json!({
            "schema_version": 1,
            "default_profile": settings.default_kernel,
            "profiles": routes,
        }))
        .map_err(io::Error::other)?,
    )?;
    let compose = format!(
        "name: didaction\nservices:\n{services}  gateway:\n    image: ghcr.io/didaction/didaction-jupyter-fork-gateway:latest\n    depends_on: [{dependencies}]\n    environment:\n      DIDACTION_KERNEL_PROFILES_FILE: /run/didaction/kernel-profiles.json\n      DIDACTION_JUPYTER_TOKEN_FILE: /run/secrets/jupyter_token\n      DIDACTION_NOTEBOOK_PATH: notebook.ipynb\n    volumes:\n      - {profiles}:/run/didaction/kernel-profiles.json:ro\n    ports: [\"127.0.0.1:{port}:8080\"]\n    secrets: [jupyter_token]\n    security_opt: [no-new-privileges:true]\n    cap_drop: [ALL]\nsecrets:\n  jupyter_token:\n    file: {directory}/secrets/jupyter-token\n",
        dependencies = dependencies.join(", "),
        profiles = directory.join("kernel-profiles.json").display(),
        port = settings.port,
        directory = directory.display(),
    );
    fs::write(directory.join("compose.yaml"), compose)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
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
        settings.kernels.get_mut("tlaplus").unwrap().enabled = true;
        write(&settings, &directory).unwrap();
        let compose = fs::read_to_string(directory.join("compose.yaml")).unwrap();
        assert!(compose.contains("  jupyter-verus:\n"));
        assert!(compose.contains("hostname: jupyter-verus"));
        assert!(compose.contains("hostname: jupyter-tlaplus"));
        assert!(compose.contains("exec start-notebook.py"));
        assert!(compose.contains("DIDACTION_KERNEL_PROFILES_FILE"));
        let profiles: serde_json::Value =
            serde_json::from_slice(&fs::read(directory.join("kernel-profiles.json")).unwrap())
                .unwrap();
        assert_eq!(profiles["profiles"]["verus"]["kernelspec"], "verus");
        assert_eq!(
            profiles["profiles"]["tlaplus"]["kernelspec"],
            "tlaplus_jupyter"
        );
        assert!(directory.join("notebooks/notebook.ipynb").is_file());
        fs::remove_dir_all(directory).unwrap();
    }
}
