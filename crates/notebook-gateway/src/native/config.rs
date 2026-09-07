use super::{Result, error};
use notebook_protocol::ErrorCode;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashSet},
    env,
    path::PathBuf,
    time::Duration,
};
use url::Url;

#[derive(Clone, Debug)]
pub struct KernelProfile {
    pub url: Url,
    pub kernelspec: String,
}
#[derive(Deserialize)]
struct KernelProfilesFile {
    schema_version: u32,
    default_profile: String,
    profiles: BTreeMap<String, RawKernelProfile>,
}
#[derive(Deserialize)]
struct RawKernelProfile {
    url: String,
    kernelspec: String,
}
pub struct Config {
    pub url: Url,
    pub token: String,
    pub kernel: String,
    pub default_profile: String,
    pub kernel_profiles: BTreeMap<String, KernelProfile>,
    pub notebook: String,
    pub workspace: PathBuf,
    pub workspace_label: String,
    pub static_dir: Option<PathBuf>,
    pub listen: String,
    pub request_limit: usize,
    pub response_limit: usize,
    pub timeout: Duration,
    pub allowed_origins: HashSet<String>,
}
fn value(key: &str, default: &str) -> String {
    env::var(format!("DIDACTION_{key}")).unwrap_or_else(|_| default.into())
}
fn invalid() -> notebook_protocol::ProtocolError {
    error(ErrorCode::InvalidInput, "Invalid gateway configuration")
}
impl Config {
    pub fn load() -> Result<Self> {
        let mut url =
            Url::parse(&value("JUPYTER_URL", "http://127.0.0.1:8888")).map_err(|_| invalid())?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(invalid());
        }
        url.set_path(&format!("{}/", url.path().trim_end_matches('/')));
        let token = match env::var("DIDACTION_JUPYTER_TOKEN_FILE") {
            Ok(path) => std::fs::read_to_string(path)
                .map_err(|_| invalid())?
                .trim()
                .into(),
            Err(_) => value("JUPYTER_TOKEN", ""),
        };
        if token.len() > 4096 || token.contains(['\r', '\n']) {
            return Err(invalid());
        }
        let request_limit = value("REQUEST_LIMIT", "300000")
            .parse()
            .map_err(|_| invalid())?;
        let response_limit = value("RESPONSE_LIMIT", "4000000")
            .parse()
            .map_err(|_| invalid())?;
        let seconds: f64 = value("TIMEOUT_SECONDS", "30")
            .parse()
            .map_err(|_| invalid())?;
        if !(1..=4_000_000).contains(&request_limit)
            || !(1..=8_000_000).contains(&response_limit)
            || !(0.1..=120.0).contains(&seconds)
        {
            return Err(invalid());
        }
        let kernel = value("KERNEL_NAME", "python3");
        let (default_profile, kernel_profiles) = match env::var("DIDACTION_KERNEL_PROFILES_FILE") {
            Ok(path) => parse_profiles(&std::fs::read(path).map_err(|_| invalid())?)?,
            Err(_) => {
                let default_profile = "default".to_string();
                let profiles = BTreeMap::from([(
                    default_profile.clone(),
                    KernelProfile {
                        url: url.clone(),
                        kernelspec: kernel.clone(),
                    },
                )]);
                (default_profile, profiles)
            }
        };
        let default = kernel_profiles.get(&default_profile).ok_or_else(invalid)?;
        let workspace = PathBuf::from(value("WORKSPACE", ".runtime/notebooks"));
        let workspace_label = value("WORKSPACE_LABEL", &workspace.display().to_string());
        if workspace_label.is_empty() || workspace_label.len() > 4096 {
            return Err(invalid());
        }
        let config = Self {
            url: default.url.clone(),
            token,
            kernel: default.kernelspec.clone(),
            default_profile,
            kernel_profiles,
            notebook: value("NOTEBOOK_PATH", "notebook-parity-demo.ipynb"),
            workspace,
            workspace_label,
            static_dir: env::var("DIDACTION_STATIC_DIR").ok().map(Into::into),
            listen: value("GATEWAY_BIND", "127.0.0.1:8080"),
            request_limit,
            response_limit,
            timeout: Duration::from_secs_f64(seconds),
            allowed_origins: parse_origins(&value("ALLOWED_ORIGINS", ""))?,
        };
        config.path(&config.notebook, false)?;
        if config.kernel.is_empty() || config.kernel.len() > 128 {
            return Err(invalid());
        }
        Ok(config)
    }
    pub fn origin_allowed(&self, raw: &str, host: &str) -> bool {
        normalize_origin(raw).is_ok_and(|origin| {
            self.allowed_origins.contains(&origin)
                || Url::parse(raw).is_ok_and(|url| {
                    url[url::Position::BeforeHost..url::Position::AfterPort] == *host
                })
        })
    }
    pub fn path(&self, raw: &str, directory: bool) -> Result<String> {
        confined(raw, directory)?;
        // Local deployments also reject existing escaping symlinks. In sidecar
        // deployments the configured Jupyter Contents manager enforces this.
        if let Ok(root) = self.workspace.canonicalize() {
            let mut candidate = root.join(raw);
            while !candidate.exists() {
                if !candidate.pop() {
                    break;
                }
            }
            if let Ok(resolved) = candidate.canonicalize()
                && !resolved.starts_with(&root)
            {
                return Err(error(
                    ErrorCode::PathRejected,
                    "Path is outside the workspace",
                ));
            }
        }
        Ok(if directory || raw.ends_with(".ipynb") {
            raw.into()
        } else {
            format!("{raw}.ipynb")
        })
    }
}
fn parse_profiles(raw: &[u8]) -> Result<(String, BTreeMap<String, KernelProfile>)> {
    let file: KernelProfilesFile = serde_json::from_slice(raw).map_err(|_| invalid())?;
    if file.schema_version != 1
        || file.profiles.is_empty()
        || !file.profiles.contains_key(&file.default_profile)
    {
        return Err(invalid());
    }
    let profiles = file
        .profiles
        .into_iter()
        .map(|(id, raw)| {
            let mut url = Url::parse(&raw.url).map_err(|_| invalid())?;
            url.set_path(&format!("{}/", url.path().trim_end_matches('/')));
            let profile = KernelProfile {
                url,
                kernelspec: raw.kernelspec,
            };
            validate_profile(&id, &profile)?;
            Ok((id, profile))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    Ok((file.default_profile, profiles))
}
fn validate_profile(id: &str, profile: &KernelProfile) -> Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        || profile.kernelspec.is_empty()
        || profile.kernelspec.len() > 128
        || !matches!(profile.url.scheme(), "http" | "https")
        || profile.url.host_str().is_none()
        || !profile.url.username().is_empty()
        || profile.url.password().is_some()
        || profile.url.query().is_some()
        || profile.url.fragment().is_some()
    {
        return Err(invalid());
    }
    Ok(())
}
fn normalize_origin(raw: &str) -> Result<String> {
    let url = Url::parse(raw).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid());
    }
    Ok(url.origin().ascii_serialization())
}
fn parse_origins(raw: &str) -> Result<HashSet<String>> {
    raw.split(',')
        .map(str::trim)
        .filter(|origin| !origin.is_empty())
        .map(normalize_origin)
        .collect()
}
pub fn confined(raw: &str, directory: bool) -> Result<()> {
    if directory && raw.is_empty() {
        return Ok(());
    }
    if raw.is_empty()
        || raw.len() > 512
        || raw.chars().any(|c| c.is_control() || "\\%?#:".contains(c))
        || raw
            .split('/')
            .any(|part| part.is_empty() || part.starts_with('.'))
    {
        return Err(error(
            ErrorCode::PathRejected,
            "Choose a path inside the configured workspace",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn confinement() {
        for path in [
            "/x", "../x", "a/../b", "a//b", "a\\b", "a%2fb", ".secret", "a?b", "a\nb", "a:b",
        ] {
            assert!(confined(path, false).is_err(), "{path}");
        }
        assert!(confined("lesson/λ.ipynb", false).is_ok());
        assert!(confined("", true).is_ok());
    }
    #[test]
    fn origins_are_exact_and_bounded_to_http_origins() {
        let origins = parse_origins("https://notebooks.example, http://localhost:5173").unwrap();
        assert!(origins.contains("https://notebooks.example"));
        assert!(origins.contains("http://localhost:5173"));
        for invalid in [
            "*",
            "https://example.com/path",
            "file:///tmp/app",
            "https://u:p@example.com",
        ] {
            assert!(parse_origins(invalid).is_err(), "{invalid}");
        }
    }
    #[test]
    fn profile_file_is_versioned_bounded_and_normalizes_urls() {
        let (default, profiles) = parse_profiles(br#"{"schema_version":1,"default_profile":"verus","profiles":{"verus":{"url":"http://jupyter-verus:8888","kernelspec":"verus"},"tlaplus":{"url":"http://jupyter-tlaplus:8888","kernelspec":"tlaplus_jupyter"}}}"#).unwrap();
        assert_eq!(default, "verus");
        assert_eq!(
            profiles["tlaplus"].url.as_str(),
            "http://jupyter-tlaplus:8888/"
        );
        for invalid in [br#"{"schema_version":2,"default_profile":"x","profiles":{}}"#.as_slice(), br#"{"schema_version":1,"default_profile":"missing","profiles":{"x":{"url":"file:///tmp/x","kernelspec":"x"}}}"#.as_slice()] {
            assert!(parse_profiles(invalid).is_err());
        }
    }
}
