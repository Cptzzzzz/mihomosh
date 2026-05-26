#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    env, fs,
    io::{Cursor, Read, Write},
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use flate2::read::GzDecoder;
use reqwest::ClientBuilder;
use serde::Deserialize;
use serde_yml::{Mapping, Value};
use sha2::{Digest, Sha256};
use tar::Archive;
use tempfile::TempDir;
use url::Url;

use crate::{
    commands::sub::{self, SUBSCRIPTION_SECTIONS},
    includes::DEFAULT_MIHOMO_CONFIG_TEMPLATE,
    models::config::Config,
    println_success,
};

const MIHOMO_LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/MetaCubeX/mihomo/releases/latest";
const WEB_UI_TARBALL_URLS: [&str; 2] = [
    "https://codeload.github.com/MetaCubeX/Yacd-meta/tar.gz/refs/heads/gh-pages",
    "https://codeload.github.com/MetaCubeX/metacubexd/tar.gz/refs/heads/gh-pages",
];
const GEOIP_METADB_URL: &str =
    "https://github.com/MetaCubeX/meta-rules-dat/releases/download/latest/geoip.metadb";
const MIHOMO_SERVICE_NAME: &str = "mihomo";

pub async fn handle_init(url: Option<Url>) -> Result<()> {
    let cfg = Config::get_instance();
    let paths = InitPaths::new(cfg.mihomo_path.clone());
    init_mihomo(url, &paths).await
}

async fn init_mihomo(url: Option<Url>, paths: &InitPaths) -> Result<()> {
    ensure_linux()?;

    let client = ClientBuilder::new()
        .timeout(Duration::from_secs(120))
        .user_agent(format!(
            "mihomosh/v{} (clash-verge)",
            env!("CARGO_PKG_VERSION")
        ))
        .build()
        .context("Fail to build reqwest client")?;

    let config = prepare_config(url, paths)
        .await
        .context("Fail to prepare Mihomo config")?;
    write_file(&paths.config_path, config.as_bytes()).with_context(|| {
        format!(
            "Fail to write Mihomo config `{}`",
            paths.config_path.display()
        )
    })?;

    install_mihomo_binary(&client, &paths.binary_path)
        .await
        .context("Fail to install Mihomo binary")?;
    install_web_ui(&client, &paths.web_ui_dir)
        .await
        .context("Fail to install Mihomo Web UI")?;
    install_geoip_db(&client, &paths.config_dir)
        .await
        .context("Fail to install Mihomo GEOIP database")?;

    let unit = build_systemd_unit(paths);
    write_file(&paths.systemd_unit_path, unit.as_bytes()).with_context(|| {
        format!(
            "Fail to write systemd unit `{}`",
            paths.systemd_unit_path.display()
        )
    })?;

    test_mihomo_config(paths).context("Fail to test Mihomo config")?;
    run_systemctl(paths, &["daemon-reload"])?;
    run_systemctl(paths, &["enable", MIHOMO_SERVICE_NAME])?;
    run_systemctl(paths, &["restart", MIHOMO_SERVICE_NAME])?;

    println_success!("Mihomo initialized");
    Ok(())
}

async fn prepare_config(url: Option<Url>, paths: &InitPaths) -> Result<String> {
    let mut config = if paths.config_path.is_file() {
        let current = fs::read_to_string(&paths.config_path).with_context(|| {
            format!(
                "Fail to read current Mihomo config `{}`",
                paths.config_path.display()
            )
        })?;
        let current = serde_yml::from_str::<Value>(&current).with_context(|| {
            format!(
                "Fail to parse current Mihomo config `{}`",
                paths.config_path.display()
            )
        })?;
        let sections = normalize_subscription_sections(&current)?;
        sub::replace_subscription_sections(DEFAULT_MIHOMO_CONFIG_TEMPLATE, &sections)?
    } else {
        DEFAULT_MIHOMO_CONFIG_TEMPLATE.to_owned()
    };

    if let Some(url) = url {
        let subscription = sub::fetch_meta_subscription_config(url).await?;
        config = sub::replace_subscription_sections(&config, &subscription)?;
    }

    serde_yml::from_str::<Value>(&config).context("Prepared Mihomo config is not valid YAML")?;
    Ok(config)
}

fn normalize_subscription_sections(config: &Value) -> Result<Value> {
    let config = config
        .as_mapping()
        .ok_or_else(|| anyhow!("Current Mihomo config must be a YAML object"))?;
    let mut sections = Mapping::new();

    for section in SUBSCRIPTION_SECTIONS {
        let key = Value::String(section.to_owned());
        let value = match config.get(&key) {
            Some(value) if value.is_sequence() => value.clone(),
            _ => Value::Sequence(Vec::new()),
        };
        sections.insert(key, value);
    }

    Ok(Value::Mapping(sections))
}

async fn install_mihomo_binary(client: &reqwest::Client, binary_path: &Path) -> Result<()> {
    let release = client
        .get(MIHOMO_LATEST_RELEASE_API)
        .send()
        .await
        .context("Fail to request latest Mihomo release")?
        .error_for_status()
        .context("Fail to request latest Mihomo release")?
        .json::<GithubRelease>()
        .await
        .context("Fail to parse latest Mihomo release")?;
    let asset = select_mihomo_asset(&release)?;

    let compressed = download(client, &asset.browser_download_url)
        .await
        .with_context(|| format!("Fail to download `{}`", asset.name))?;
    verify_digest(&compressed, asset.digest.as_deref())
        .with_context(|| format!("Fail to verify `{}`", asset.name))?;

    let mut decoder = GzDecoder::new(Cursor::new(compressed));
    let mut binary = Vec::new();
    decoder
        .read_to_end(&mut binary)
        .context("Fail to decompress Mihomo binary")?;

    write_executable(binary_path, &binary)?;
    Ok(())
}

async fn install_web_ui(client: &reqwest::Client, web_ui_dir: &Path) -> Result<()> {
    let mut last_err = None;
    for url in WEB_UI_TARBALL_URLS {
        match download(client, url).await {
            Ok(data) => return install_web_ui_archive(&data, web_ui_dir),
            Err(err) => last_err = Some(err),
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("No Mihomo Web UI download URL available")))
        .context("Fail to download Mihomo Web UI")
}

fn install_web_ui_archive(data: &[u8], web_ui_dir: &Path) -> Result<()> {
    let parent = web_ui_dir
        .parent()
        .ok_or_else(|| anyhow!("Web UI path `{}` has no parent", web_ui_dir.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("Fail to create directory `{}`", parent.display()))?;

    let tmp = TempDir::new_in(parent).with_context(|| {
        format!(
            "Fail to create temporary directory in `{}`",
            parent.display()
        )
    })?;
    let mut archive = Archive::new(GzDecoder::new(Cursor::new(data)));
    archive
        .unpack(tmp.path())
        .context("Fail to unpack Mihomo Web UI")?;

    let extracted = first_child_dir(tmp.path()).context("Fail to locate unpacked Web UI")?;
    remove_path(web_ui_dir)?;
    fs::rename(&extracted, web_ui_dir).with_context(|| {
        format!(
            "Fail to move Web UI from `{}` to `{}`",
            extracted.display(),
            web_ui_dir.display()
        )
    })?;

    Ok(())
}

async fn install_geoip_db(client: &reqwest::Client, config_dir: &Path) -> Result<()> {
    let path = config_dir.join("geoip.metadb");
    if path.is_file() && path.metadata().map_or(false, |metadata| metadata.len() > 0) {
        return Ok(());
    }

    let data = download(client, GEOIP_METADB_URL)
        .await
        .context("Fail to download GEOIP database")?;
    write_file(&path, &data)
}

async fn download(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let mut last_err = None;
    for url in download_candidates(url) {
        for _ in 0..3 {
            match download_once(client, &url).await {
                Ok(data) => return Ok(data),
                Err(err) => last_err = Some(err),
            }
            thread::sleep(Duration::from_secs(2));
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("No download attempt was made")))
}

fn download_candidates(url: &str) -> Vec<String> {
    let mut urls = vec![url.to_owned()];
    if url.starts_with("https://github.com/") || url.starts_with("https://codeload.github.com/") {
        urls.push(format!("https://gh-proxy.com/{url}"));
        urls.push(format!("https://ghproxy.net/{url}"));
    }

    urls
}

async fn download_once(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    Ok(client
        .get(url)
        .send()
        .await
        .with_context(|| format!("Fail to send `GET {url}`"))?
        .error_for_status()
        .with_context(|| format!("Fail to request `GET {url}`"))?
        .bytes()
        .await
        .with_context(|| format!("Fail to read response from `GET {url}`"))?
        .to_vec())
}

fn select_mihomo_asset(release: &GithubRelease) -> Result<&GithubAsset> {
    let platform = mihomo_platform()?;
    let expected = format!(
        "mihomo-{}-{}-{}.gz",
        platform.os, platform.arch, release.tag_name
    );

    release
        .assets
        .iter()
        .find(|asset| asset.name == expected)
        .ok_or_else(|| {
            anyhow!(
                "Mihomo release `{}` does not contain `{expected}`",
                release.tag_name
            )
        })
}

fn mihomo_platform() -> Result<MihomoPlatform> {
    if env::consts::OS != "linux" {
        bail!("`mihomosh init` currently supports Linux/systemd only");
    }

    let arch = match env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "arm" => "armv7",
        "x86" => "386",
        arch => bail!("Unsupported Linux architecture `{arch}`"),
    };

    Ok(MihomoPlatform { os: "linux", arch })
}

fn verify_digest(data: &[u8], digest: Option<&str>) -> Result<()> {
    let Some(digest) = digest else {
        return Ok(());
    };
    let Some(expected) = digest.strip_prefix("sha256:") else {
        return Ok(());
    };

    let actual = hex::encode(Sha256::digest(data));
    if actual != expected {
        bail!("sha256 mismatch: expected `{expected}`, got `{actual}`");
    }

    Ok(())
}

fn write_executable(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("Path `{}` has no parent", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("Fail to create directory `{}`", parent.display()))?;

    let tmp_path = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("mihomo"),
        std::process::id()
    ));
    {
        let mut file = fs::File::create(&tmp_path)
            .with_context(|| format!("Fail to create file `{}`", tmp_path.display()))?;
        file.write_all(data)
            .with_context(|| format!("Fail to write file `{}`", tmp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("Fail to sync file `{}`", tmp_path.display()))?;
    }
    #[cfg(unix)]
    fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("Fail to chmod file `{}`", tmp_path.display()))?;
    fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "Fail to move file from `{}` to `{}`",
            tmp_path.display(),
            path.display()
        )
    })?;

    Ok(())
}

fn write_file(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("Path `{}` has no parent", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("Fail to create directory `{}`", parent.display()))?;
    fs::write(path, data).with_context(|| format!("Fail to write file `{}`", path.display()))
}

fn remove_path(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    if path.is_dir() {
        fs::remove_dir_all(path)
            .with_context(|| format!("Fail to remove directory `{}`", path.display()))?;
    } else {
        fs::remove_file(path)
            .with_context(|| format!("Fail to remove file `{}`", path.display()))?;
    }

    Ok(())
}

fn first_child_dir(path: &Path) -> Result<PathBuf> {
    for entry in fs::read_dir(path).with_context(|| format!("Fail to read `{}`", path.display()))? {
        let entry = entry.with_context(|| format!("Fail to read `{}`", path.display()))?;
        if entry
            .file_type()
            .with_context(|| format!("Fail to stat `{}`", entry.path().display()))?
            .is_dir()
        {
            return Ok(entry.path());
        }
    }

    bail!("No child directory found in `{}`", path.display())
}

fn build_systemd_unit(paths: &InitPaths) -> String {
    format!(
        "[Unit]\n\
Description=Mihomo daemon\n\
After=network-online.target\n\
Wants=network-online.target\n\
\n\
[Service]\n\
Type=simple\n\
ExecStart={} -d {} -f {}\n\
Restart=on-failure\n\
RestartSec=5\n\
LimitNOFILE=1048576\n\
\n\
[Install]\n\
WantedBy=multi-user.target\n",
        paths.binary_path.display(),
        paths.config_dir.display(),
        paths.config_path.display()
    )
}

fn test_mihomo_config(paths: &InitPaths) -> Result<()> {
    run_command(
        &paths.binary_path,
        &[
            "-t",
            "-d",
            path_to_str(&paths.config_dir)?,
            "-f",
            path_to_str(&paths.config_path)?,
        ],
    )
}

fn run_systemctl(paths: &InitPaths, args: &[&str]) -> Result<()> {
    run_command(&paths.systemctl_path, args)
}

fn run_command(program: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("Fail to execute `{}`", program.display()))?;
    if !status.success() {
        bail!(
            "Command `{}` failed with status {status}",
            program.display()
        );
    }

    Ok(())
}

fn path_to_str(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow!("Path `{}` is not valid UTF-8", path.display()))
}

fn ensure_linux() -> Result<()> {
    if env::consts::OS != "linux" {
        bail!("`mihomosh init` currently supports Linux/systemd only");
    }
    Ok(())
}

struct InitPaths {
    config_dir: PathBuf,
    config_path: PathBuf,
    binary_path: PathBuf,
    systemd_unit_path: PathBuf,
    web_ui_dir: PathBuf,
    systemctl_path: PathBuf,
}

impl InitPaths {
    fn new(config_path: PathBuf) -> Self {
        let root = env::var_os("MIHOMOSH_INIT_ROOT").map(PathBuf::from);
        let config_path = env::var_os("MIHOMOSH_MIHOMO_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| apply_root(root.as_deref(), &config_path));
        let config_dir = config_path
            .parent()
            .map(Path::to_owned)
            .unwrap_or_else(|| apply_root(root.as_deref(), Path::new("/etc/mihomo")));
        let binary_path = env::var_os("MIHOMOSH_MIHOMO_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| apply_root(root.as_deref(), Path::new("/usr/local/bin/mihomo")));
        let systemd_unit_path = env::var_os("MIHOMOSH_SYSTEMD_UNIT")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                apply_root(
                    root.as_deref(),
                    Path::new("/etc/systemd/system/mihomo.service"),
                )
            });
        let web_ui_dir = config_dir.join("web-ui");
        let systemctl_path = env::var_os("MIHOMOSH_SYSTEMCTL")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("systemctl"));

        Self {
            config_dir,
            config_path,
            binary_path,
            systemd_unit_path,
            web_ui_dir,
            systemctl_path,
        }
    }
}

fn apply_root(root: Option<&Path>, path: &Path) -> PathBuf {
    let Some(root) = root else {
        return path.to_owned();
    };
    match path.strip_prefix("/") {
        Ok(path) => root.join(path),
        Err(_) => root.join(path),
    }
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

#[derive(Deserialize)]
struct GithubAsset {
    name: String,
    digest: Option<String>,
    browser_download_url: String,
}

struct MihomoPlatform {
    os: &'static str,
    arch: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(input: &str) -> Value {
        serde_yml::from_str(input).unwrap()
    }

    fn get<'a>(value: &'a Value, key: &str) -> &'a Value {
        let key = Value::String(key.to_owned());
        value.as_mapping().unwrap().get(&key).unwrap()
    }

    #[test]
    fn normalize_subscription_sections_keeps_only_subscription_sections() {
        let config = yaml(
            r#"
mixed-port: 7890
proxies:
  - name: keep
    type: direct
proxy-groups:
  - name: group
    type: select
    proxies:
      - keep
rules:
  - MATCH,keep
"#,
        );

        let sections = normalize_subscription_sections(&config).unwrap();

        assert!(get(&sections, "proxies").is_sequence());
        assert!(get(&sections, "proxy-groups").is_sequence());
        assert!(get(&sections, "rules").is_sequence());
        assert!(
            !sections
                .as_mapping()
                .unwrap()
                .contains_key(&Value::String("mixed-port".to_owned()))
        );
    }

    #[test]
    fn normalize_subscription_sections_uses_empty_lists_when_missing() {
        let sections = normalize_subscription_sections(&yaml("{}")).unwrap();

        assert_eq!(get(&sections, "proxies"), &Value::Sequence(Vec::new()));
        assert_eq!(get(&sections, "proxy-groups"), &Value::Sequence(Vec::new()));
        assert_eq!(get(&sections, "rules"), &Value::Sequence(Vec::new()));
    }

    #[test]
    fn build_systemd_unit_uses_config_and_binary_paths() {
        let paths = InitPaths {
            config_dir: PathBuf::from("/etc/mihomo"),
            config_path: PathBuf::from("/etc/mihomo/config.yaml"),
            binary_path: PathBuf::from("/usr/local/bin/mihomo"),
            systemd_unit_path: PathBuf::from("/etc/systemd/system/mihomo.service"),
            web_ui_dir: PathBuf::from("/etc/mihomo/web-ui"),
            systemctl_path: PathBuf::from("systemctl"),
        };

        let unit = build_systemd_unit(&paths);

        assert!(unit.contains(
            "ExecStart=/usr/local/bin/mihomo -d /etc/mihomo -f /etc/mihomo/config.yaml\n"
        ));
        assert!(unit.contains("WantedBy=multi-user.target\n"));
    }

    #[test]
    fn select_mihomo_asset_uses_exact_linux_asset_name() {
        let release = GithubRelease {
            tag_name: "v1.2.3".to_owned(),
            assets: vec![
                GithubAsset {
                    name: "mihomo-linux-arm64-v1.2.3.deb".to_owned(),
                    digest: None,
                    browser_download_url: "deb".to_owned(),
                },
                GithubAsset {
                    name: format!("mihomo-linux-{}-v1.2.3.gz", mihomo_platform().unwrap().arch),
                    digest: Some("sha256:test".to_owned()),
                    browser_download_url: "gz".to_owned(),
                },
            ],
        };

        assert_eq!(
            select_mihomo_asset(&release).unwrap().browser_download_url,
            "gz"
        );
    }

    #[test]
    fn verify_digest_checks_sha256() {
        let data = b"mihomo";
        let digest = format!("sha256:{}", hex::encode(Sha256::digest(data)));

        verify_digest(data, Some(&digest)).unwrap();
        assert!(verify_digest(data, Some("sha256:bad")).is_err());
    }
}
