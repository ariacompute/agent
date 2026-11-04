//! Upgrade the `aria-agent` CLI binary + `libaria-agent_ffi` cdylib from
//! GitHub/Gitee Releases.
//!
//! Mirrors `aria-router`'s `bin/src/upgrade.rs`, adapted to the agent repo: the
//! binary asset is `aria-agent`, the FFI asset is `libaria-agent_ffi`, and the
//! Releases repo is `agent` (under the org root in `upgrade_url`).
//!
//! Everything here takes explicit paths (`InstallTargets`) so the whole flow —
//! API discovery, download, extraction and installation — can be exercised by
//! tests against a local HTTP stub. The only environment glue (CLI config,
//! `current_exe`, home directory) lives in `src/main.rs`.

use futures::StreamExt;
use serde::Deserialize;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAsset {
    pub name: String,
    pub download_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseInfo {
    pub tag: String,
    pub prerelease: bool,
    pub draft: bool,
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Debug, Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseHost {
    GitHub,
    Gitee,
}

const AGENT_REPO: &str = "agent";

/// Where an upgrade installs to. Injected (rather than read from the
/// environment) so tests never touch the running binary or the real
/// `~/.ariacompute`.
#[derive(Debug, Clone)]
pub struct InstallTargets {
    /// Root under which the per-version staging directory is created.
    pub staging_root: PathBuf,
    /// Destination of the replaced CLI binary.
    pub cli_dest: PathBuf,
    /// Directory the FFI library is installed into.
    pub lib_dir: PathBuf,
}

/// Download, verify-by-name, extract and install `version` (or the latest
/// stable release) for the platform this binary was built for.
///
/// `api_base` overrides the Releases API host (used by tests); `None` means the
/// host implied by `org_url` (GitHub / Gitee).
pub async fn upgrade_into(
    targets: &InstallTargets,
    org_url: &str,
    api_base: Option<&str>,
    version: Option<&str>,
    current_version: &str,
) -> io::Result<()> {
    let org = org_url.trim_end_matches('/');
    let host = detect_host(org)?;
    let releases = fetch_releases(host, org, api_base).await?;
    let target = select_release(&releases, version)?;
    let ver = strip_v(&target.tag);
    let current = strip_v(current_version);
    if ver == current {
        println!("already at {ver}");
        return Ok(());
    }

    let asset_os = detect_asset_os()?;
    let cli_name = agent_asset_name(&ver, asset_os);
    let ffi_name = format!("libaria-agent_ffi_{ver}_{asset_os}.tar.gz");
    let cli_asset = find_asset(&target.assets, &cli_name)?;
    let ffi_asset = find_asset(&target.assets, &ffi_name)?;

    // Start from a clean staging directory: a stale one left by an interrupted
    // upgrade is disposable, but any real removal failure must surface.
    let staging = targets.staging_root.join(format!("tmp/upgrade-{ver}"));
    fs::remove_dir_all(&staging).or_else(|e| match e.kind() {
        io::ErrorKind::NotFound => Ok(()),
        _ => Err(e),
    })?;
    fs::create_dir_all(&staging)?;

    let cli_archive = staging.join(&cli_name);
    let ffi_archive = staging.join(&ffi_name);
    download_url(
        &cli_asset.download_url,
        &cli_archive,
        &format!("aria-agent {ver}"),
    )
    .await?;
    download_url(
        &ffi_asset.download_url,
        &ffi_archive,
        &format!("libaria-agent_ffi {ver}"),
    )
    .await?;

    let cli_extract = staging.join("cli");
    let ffi_extract = staging.join("ffi");
    fs::create_dir_all(&cli_extract)?;
    fs::create_dir_all(&ffi_extract)?;
    extract_archive(&cli_archive, &cli_extract)?;
    extract_archive(&ffi_archive, &ffi_extract)?;

    install_cli(&cli_extract, &targets.cli_dest)?;
    install_ffi(&ffi_extract, &targets.lib_dir)?;

    let _ = fs::remove_dir_all(&staging);
    println!("upgraded to {ver}");
    println!(
        "libaria-agent_ffi installed under {} (set ARIA_AGENT_FFI_LIB if needed)",
        targets.lib_dir.display()
    );
    Ok(())
}

/// Pick the upgrade source: `--url` wins over the CLI config; an empty result
/// is a user error (point them at `setup`).
pub fn resolve_upgrade_url(url_override: Option<&str>, configured: &str) -> io::Result<String> {
    let url = match url_override {
        Some(u) if !u.trim().is_empty() => u.trim(),
        _ => configured.trim(),
    };
    if url.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "upgrade_url not set; run `aria-agent setup` first",
        ));
    }
    Ok(url.to_string())
}

pub fn detect_host(org_url: &str) -> io::Result<ReleaseHost> {
    let lower = org_url.to_ascii_lowercase();
    if lower.contains("gitee.com") {
        Ok(ReleaseHost::Gitee)
    } else if lower.contains("github.com") {
        Ok(ReleaseHost::GitHub)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported upgrade_url host: {org_url}"),
        ))
    }
}

/// Releases API URL. `api_base` (tests) replaces the host-specific prefix.
pub fn releases_api_url(host: ReleaseHost, org: &str, api_base: Option<&str>) -> String {
    let owner = org
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("ariacompute");
    match api_base {
        Some(base) => format!(
            "{}/repos/{owner}/{AGENT_REPO}/releases?per_page=30",
            base.trim_end_matches('/')
        ),
        None => match host {
            ReleaseHost::GitHub => {
                format!("https://api.github.com/repos/{owner}/{AGENT_REPO}/releases?per_page=30")
            }
            ReleaseHost::Gitee => {
                format!("https://gitee.com/api/v5/repos/{owner}/{AGENT_REPO}/releases?per_page=30")
            }
        },
    }
}

async fn fetch_releases(
    host: ReleaseHost,
    org: &str,
    api_base: Option<&str>,
) -> io::Result<Vec<ReleaseInfo>> {
    let url = releases_api_url(host, org, api_base);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(format!("aria-agent-upgrade/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(io_err)?;
    let resp = client.get(&url).send().await.map_err(io_err)?;
    if !resp.status().is_success() {
        return Err(io::Error::other(format!(
            "releases API {}: {}",
            resp.status(),
            url
        )));
    }
    let raw: Vec<GhRelease> = resp.json().await.map_err(io_err)?;
    Ok(raw
        .into_iter()
        .map(|r| ReleaseInfo {
            tag: r.tag_name,
            prerelease: r.prerelease,
            draft: r.draft,
            assets: r
                .assets
                .into_iter()
                .map(|a| ReleaseAsset {
                    name: a.name,
                    download_url: a.browser_download_url,
                })
                .collect(),
        })
        .collect())
}

pub fn select_release<'a>(
    releases: &'a [ReleaseInfo],
    version: Option<&str>,
) -> io::Result<&'a ReleaseInfo> {
    match version {
        None => releases
            .iter()
            .filter(|r| !r.draft && !r.prerelease)
            .filter(|r| parse_semver(&r.tag).is_some())
            .max_by_key(|r| parse_semver(&r.tag).unwrap_or((0, 0, 0)))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no stable release found")),
        Some(v) => {
            let want = normalize_tag(v);
            releases
                .iter()
                .find(|r| !r.draft && (normalize_tag(&r.tag) == want || r.tag == v))
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, format!("release {v} not found"))
                })
        }
    }
}

pub fn parse_semver(tag: &str) -> Option<(u64, u64, u64)> {
    let s = strip_v(tag);
    let core = s.split(['-', '+']).next().unwrap_or("");
    if core.is_empty() {
        return None;
    }
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

pub fn normalize_tag(v: &str) -> String {
    let t = v.trim();
    if t.starts_with('v') || t.starts_with('V') {
        t.to_string()
    } else {
        format!("v{t}")
    }
}

pub fn strip_v(v: &str) -> String {
    let t = v.trim();
    t.strip_prefix('v')
        .or_else(|| t.strip_prefix('V'))
        .unwrap_or(t)
        .to_string()
}

/// Release asset suffix for an OS/arch pair (`None` for unsupported targets).
pub fn asset_os_for(os: &str, arch: &str) -> io::Result<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Ok("linux_x86_64"),
        ("linux", "aarch64") => Ok("linux_arm64"),
        ("macos", _) => Ok("macos"),
        ("windows", "x86_64") => Ok("windows_x86_64"),
        (other_os, other_arch) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("unsupported platform {other_os}/{other_arch} for upgrade"),
        )),
    }
}

pub fn detect_asset_os() -> io::Result<&'static str> {
    asset_os_for(std::env::consts::OS, std::env::consts::ARCH)
}

pub fn agent_asset_name(version: &str, asset_os: &str) -> String {
    if asset_os.starts_with("windows") {
        format!("aria-agent_{version}_{asset_os}.zip")
    } else {
        format!("aria-agent_{version}_{asset_os}.tar.gz")
    }
}

fn find_asset<'a>(assets: &'a [ReleaseAsset], name: &str) -> io::Result<&'a ReleaseAsset> {
    assets.iter().find(|a| a.name == name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("release asset not found: {name}"),
        )
    })
}

async fn download_url(url: &str, path: &Path, label: &str) -> io::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .redirect(reqwest::redirect::Policy::limited(10))
        .user_agent(format!("aria-agent-upgrade/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(io_err)?;
    let resp = client.get(url).send().await.map_err(io_err)?;
    if !resp.status().is_success() {
        return Err(io::Error::other(format!(
            "download failed {}: {}",
            resp.status(),
            url
        )));
    }
    stream_response_to_file(resp, path, label).await?;
    Ok(())
}

async fn stream_response_to_file(
    resp: reqwest::Response,
    path: &Path,
    label: &str,
) -> io::Result<u64> {
    if !label.is_empty() {
        eprintln!("downloading {label}…");
    }
    let mut file = fs::File::create(path)?;
    let mut stream = resp.bytes_stream();
    let mut downloaded = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(io_err)?;
        file.write_all(&chunk)?;
        downloaded += chunk.len() as u64;
    }
    file.flush()?;
    Ok(downloaded)
}

fn extract_archive(archive: &Path, dest: &Path) -> io::Result<()> {
    let name = archive
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if name.ends_with(".zip") {
        extract_zip(archive, dest)
    } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        extract_tar_gz(archive, dest)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown archive type: {name}"),
        ))
    }
}

fn extract_zip(archive: &Path, dest: &Path) -> io::Result<()> {
    let file = fs::File::open(archive)?;
    let mut zip =
        zip::ZipArchive::new(file).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let out_path = match entry.enclosed_name() {
            Some(p) => dest.join(p),
            None => continue,
        };
        if entry.is_dir() {
            fs::create_dir_all(&out_path)?;
            continue;
        }
        out_path.parent().map(fs::create_dir_all).transpose()?;
        let mut out = fs::File::create(&out_path)?;
        io::copy(&mut entry, &mut out)?;
    }
    Ok(())
}

fn extract_tar_gz(archive: &Path, dest: &Path) -> io::Result<()> {
    let file = fs::File::open(archive)?;
    let dec = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(dec);
    archive.unpack(dest)?;
    Ok(())
}

/// Name of the CLI binary inside the release archive (platform specific).
#[cfg(windows)]
const CLI_BIN: &str = "aria-agent.exe";
#[cfg(not(windows))]
const CLI_BIN: &str = "aria-agent";

fn install_cli(extract_dir: &Path, dest: &Path) -> io::Result<()> {
    let dest_dir = dest
        .parent()
        .ok_or_else(|| io::Error::other("cli destination has no parent"))?;
    install_cli_with_tmp(extract_dir, dest, dest_dir)
}

/// Install the CLI binary, staging the copy in `tmp_dir`.
///
/// `tmp_dir` is a parameter (not derived from `dest`) so the cross-device
/// fallback below is reachable — `fs::rename` fails with `EXDEV` when the
/// staging directory and the destination live on different filesystems.
fn install_cli_with_tmp(extract_dir: &Path, dest: &Path, tmp_dir: &Path) -> io::Result<()> {
    let src = find_named_file(extract_dir, CLI_BIN)?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, format!("{CLI_BIN} not in archive"))
    })?;
    let tmp = tmp_dir.join(format!(".aria-agent-upgrade-{}.tmp", std::process::id()));
    fs::copy(&src, &tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&tmp, perms)?;
    }
    match fs::rename(&tmp, dest) {
        Ok(()) => Ok(()),
        Err(rename_err) => {
            // Cross-device (or otherwise un-renameable) destination: fall back
            // to a plain copy and clean up the staging file.
            let copied = fs::copy(&tmp, dest);
            let _ = fs::remove_file(&tmp);
            match copied {
                Ok(_) => Ok(()),
                Err(_) => Err(rename_err),
            }
        }
    }
}

fn install_ffi(extract_dir: &Path, lib: &Path) -> io::Result<()> {
    fs::create_dir_all(lib)?;
    let names = [
        "libaria-agent_ffi.so",
        "libaria-agent_ffi.dylib",
        "libaria-agent_ffi.a",
        "aria-agent_ffi.dll",
        "aria-agent_ffi.dll.lib",
        "aria-agent_ffi.lib",
    ];
    let mut any = false;
    for name in names {
        if let Some(src) = find_named_file(extract_dir, name)? {
            fs::copy(&src, lib.join(name))?;
            any = true;
        }
    }
    if !any {
        // Fallback: accept a versioned/renamed library bundled by the release.
        for entry in walk_files(extract_dir)? {
            let name = entry
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            if name.contains("aria-agent_ffi") || name.contains("libaria-agent_ffi") {
                fs::copy(&entry, lib.join(&name))?;
                any = true;
            }
        }
    }
    if !any {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no libaria-agent_ffi library found in archive",
        ));
    }
    Ok(())
}

fn find_named_file(root: &Path, name: &str) -> io::Result<Option<PathBuf>> {
    for path in walk_files(root)? {
        if path.file_name().and_then(|s| s.to_str()) == Some(name) {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

fn walk_files(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out)?;
            } else {
                out.push(path);
            }
        }
        Ok(())
    }
    walk(root, &mut out)?;
    Ok(out)
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn sample_releases() -> Vec<ReleaseInfo> {
        vec![
            ReleaseInfo {
                tag: "v0.8.0-rc1".into(),
                prerelease: true,
                draft: false,
                assets: vec![],
            },
            ReleaseInfo {
                tag: "v0.7.1".into(),
                prerelease: false,
                draft: false,
                assets: vec![],
            },
            ReleaseInfo {
                tag: "v0.8.0".into(),
                prerelease: false,
                draft: false,
                assets: vec![],
            },
            ReleaseInfo {
                tag: "v0.7.2".into(),
                prerelease: false,
                draft: false,
                assets: vec![
                    ReleaseAsset {
                        name: "aria-agent_0.7.2_linux_x86_64.tar.gz".into(),
                        download_url: "https://example/e".into(),
                    },
                    ReleaseAsset {
                        name: "libaria-agent_ffi_0.7.2_linux_x86_64.tar.gz".into(),
                        download_url: "https://example/f".into(),
                    },
                ],
            },
            ReleaseInfo {
                tag: "v0.9.1".into(),
                prerelease: false,
                draft: false,
                assets: vec![],
            },
        ]
    }

    // ---------------------------------------------------------- selection

    #[test]
    fn selects_latest_stable_skipping_prerelease() {
        let r = sample_releases();
        assert_eq!(select_release(&r, None).unwrap().tag, "v0.9.1");
    }

    #[test]
    fn selects_newest_by_semver_not_api_order() {
        let r = vec![
            ReleaseInfo {
                tag: "v0.8.0".into(),
                prerelease: false,
                draft: false,
                assets: vec![],
            },
            ReleaseInfo {
                tag: "v0.9.1".into(),
                prerelease: false,
                draft: false,
                assets: vec![],
            },
            ReleaseInfo {
                tag: "v0.10.0".into(),
                prerelease: false,
                draft: false,
                assets: vec![],
            },
        ];
        assert_eq!(select_release(&r, None).unwrap().tag, "v0.10.0");
        let rev: Vec<_> = r.into_iter().rev().collect();
        assert_eq!(select_release(&rev, None).unwrap().tag, "v0.10.0");
    }

    #[test]
    fn skips_drafts_and_draft_explicit_versions() {
        let r = vec![
            ReleaseInfo {
                tag: "v1.0.0".into(),
                prerelease: false,
                draft: true,
                assets: vec![],
            },
            ReleaseInfo {
                tag: "v0.5.0".into(),
                prerelease: false,
                draft: false,
                assets: vec![],
            },
        ];
        assert_eq!(select_release(&r, None).unwrap().tag, "v0.5.0");
        assert!(select_release(&r, Some("v1.0.0")).is_err());
    }

    #[test]
    fn no_stable_release_is_an_error() {
        assert!(select_release(&[], None).is_err());
        let only_pre = vec![ReleaseInfo {
            tag: "v1.0.0-rc1".into(),
            prerelease: true,
            draft: false,
            assets: vec![],
        }];
        assert!(select_release(&only_pre, None).is_err());
    }

    #[test]
    fn parse_semver_core() {
        assert_eq!(parse_semver("v0.9.1"), Some((0, 9, 1)));
        assert_eq!(parse_semver("0.8.0"), Some((0, 8, 0)));
        assert_eq!(parse_semver("v1.2.3-rc1"), Some((1, 2, 3)));
        assert_eq!(parse_semver("v1.2.3+build.5"), Some((1, 2, 3)));
        assert_eq!(parse_semver("1"), Some((1, 0, 0)));
        assert_eq!(parse_semver("1.2"), Some((1, 2, 0)));
        assert_eq!(parse_semver("not-a-version"), None);
        assert_eq!(parse_semver(""), None);
        assert_eq!(parse_semver("v"), None);
        assert_eq!(parse_semver("x.y.z"), None);
    }

    #[test]
    fn selects_by_version_with_or_without_v() {
        let r = sample_releases();
        assert_eq!(select_release(&r, Some("0.7.1")).unwrap().tag, "v0.7.1");
        assert_eq!(select_release(&r, Some("v0.7.2")).unwrap().tag, "v0.7.2");
        // A tag that already matches verbatim is accepted too.
        assert_eq!(select_release(&r, Some("v0.7.1")).unwrap().tag, "v0.7.1");
    }

    #[test]
    fn normalize_and_strip_tag() {
        assert_eq!(normalize_tag("0.7.2"), "v0.7.2");
        assert_eq!(normalize_tag("v0.7.2"), "v0.7.2");
        assert_eq!(normalize_tag("V0.7.2"), "V0.7.2");
        assert_eq!(normalize_tag(" 0.7.2 "), "v0.7.2");
        assert_eq!(strip_v("v0.7.2"), "0.7.2");
        assert_eq!(strip_v("V0.7.2"), "0.7.2");
        assert_eq!(strip_v("0.7.2"), "0.7.2");
    }

    #[test]
    fn missing_version_errors() {
        let r = sample_releases();
        assert!(select_release(&r, Some("9.9.9")).is_err());
    }

    // ------------------------------------------------------------ platform

    #[test]
    fn agent_asset_names() {
        assert_eq!(
            agent_asset_name("0.7.2", "linux_x86_64"),
            "aria-agent_0.7.2_linux_x86_64.tar.gz"
        );
        assert_eq!(
            agent_asset_name("0.7.2", "windows_x86_64"),
            "aria-agent_0.7.2_windows_x86_64.zip"
        );
    }

    #[test]
    fn asset_os_covers_every_supported_platform() {
        assert_eq!(asset_os_for("linux", "x86_64").unwrap(), "linux_x86_64");
        assert_eq!(asset_os_for("linux", "aarch64").unwrap(), "linux_arm64");
        assert_eq!(asset_os_for("macos", "aarch64").unwrap(), "macos");
        assert_eq!(asset_os_for("macos", "x86_64").unwrap(), "macos");
        assert_eq!(asset_os_for("windows", "x86_64").unwrap(), "windows_x86_64");
        let err = asset_os_for("freebsd", "riscv64").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        assert!(err.to_string().contains("freebsd/riscv64"));
        // The real platform always resolves.
        assert!(detect_asset_os().is_ok());
    }

    #[test]
    fn github_and_gitee_api_paths() {
        let gh = releases_api_url(ReleaseHost::GitHub, "https://github.com/ariacompute", None);
        assert_eq!(
            gh,
            "https://api.github.com/repos/ariacompute/agent/releases?per_page=30"
        );
        let gitee = releases_api_url(ReleaseHost::Gitee, "https://gitee.com/ariacompute/", None);
        assert_eq!(
            gitee,
            "https://gitee.com/api/v5/repos/ariacompute/agent/releases?per_page=30"
        );
        // An explicit base wins (test seam).
        assert_eq!(
            releases_api_url(
                ReleaseHost::GitHub,
                "https://github.com/aci",
                Some("http://127.0.0.1:1/")
            ),
            "http://127.0.0.1:1/repos/aci/agent/releases?per_page=30"
        );
        // A trailing slash in the org does not produce an empty owner.
        assert!(
            releases_api_url(ReleaseHost::GitHub, "https://github.com/aci/", None)
                .contains("/repos/aci/agent/")
        );
    }

    #[test]
    fn host_detection_and_unsupported_hosts() {
        assert_eq!(
            detect_host("https://github.com/aci").unwrap(),
            ReleaseHost::GitHub
        );
        assert_eq!(
            detect_host("https://GITEE.com/aci").unwrap(),
            ReleaseHost::Gitee
        );
        let err = detect_host("https://gitlab.com/aci").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("gitlab.com"));
    }

    #[test]
    fn upgrade_url_resolution() {
        assert_eq!(
            resolve_upgrade_url(Some(" https://github.com/aci "), "https://gitee.com/x").unwrap(),
            "https://github.com/aci"
        );
        assert_eq!(
            resolve_upgrade_url(None, " https://gitee.com/x ").unwrap(),
            "https://gitee.com/x"
        );
        assert_eq!(
            resolve_upgrade_url(Some("   "), "https://gitee.com/x").unwrap(),
            "https://gitee.com/x"
        );
        let err = resolve_upgrade_url(None, "").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("setup"));
    }

    // ----------------------------------------------------------- archives

    /// Build a `.tar.gz` with the given (name, bytes) files.
    fn tar_gz(dir: &Path, file_name: &str, entries: &[(&str, &[u8])]) -> PathBuf {
        let path = dir.join(file_name);
        let f = fs::File::create(&path).unwrap();
        let enc = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
        let mut tar = tar::Builder::new(enc);
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, name, &data[..]).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
        path
    }

    fn zip_file(dir: &Path, file_name: &str, entries: &[(&str, &[u8])]) -> PathBuf {
        let path = dir.join(file_name);
        let f = fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        for (name, data) in entries {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
        path
    }

    #[test]
    fn extracts_tar_gz_and_tgz() {
        let tmp = tempfile::tempdir().unwrap();
        for name in ["a.tar.gz", "b.tgz"] {
            let archive = tar_gz(
                tmp.path(),
                name,
                &[
                    ("aria-agent", b"bin"),
                    ("nested/libaria-agent_ffi.so", b"lib"),
                ],
            );
            let dest = tmp.path().join(format!("out-{name}"));
            fs::create_dir_all(&dest).unwrap();
            extract_archive(&archive, &dest).unwrap();
            assert!(dest.join("aria-agent").exists());
            assert!(dest.join("nested/libaria-agent_ffi.so").exists());
        }
    }

    #[test]
    fn extracts_zip_including_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = zip_file(
            tmp.path(),
            "bundle.zip",
            &[("dir/", b""), ("dir/aria-agent.exe", b"bin")],
        );
        let dest = tmp.path().join("out");
        fs::create_dir_all(&dest).unwrap();
        extract_archive(&archive, &dest).unwrap();
        assert!(dest.join("dir/aria-agent.exe").exists());
    }

    #[test]
    fn zip_entries_escaping_the_destination_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out");
        fs::create_dir_all(&dest).unwrap();

        // Absolute and parent-relative entry names have no `enclosed_name`, so
        // they must be skipped rather than written outside `dest`.
        for (i, name) in ["/abs-escape.txt", "../parent-escape.txt"]
            .iter()
            .enumerate()
        {
            let archive = zip_file(
                tmp.path(),
                &format!("evil-{i}.zip"),
                &[(name, b"pwned"), ("safe.txt", b"ok")],
            );
            extract_archive(&archive, &dest).unwrap();
            assert!(dest.join("safe.txt").exists(), "{name}");
            assert!(!dest.join("abs-escape.txt").exists(), "{name}");
            assert!(!tmp.path().join("parent-escape.txt").exists(), "{name}");
        }
    }

    #[test]
    fn rejects_unknown_archive_types_and_bad_archives() {
        let tmp = tempfile::tempdir().unwrap();
        let bogus = tmp.path().join("pkg.tar.xz");
        fs::write(&bogus, b"not an archive").unwrap();
        let err = extract_archive(&bogus, tmp.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("unknown archive type"));

        let corrupt = tmp.path().join("corrupt.zip");
        fs::write(&corrupt, b"not a zip").unwrap();
        let err = extract_archive(&corrupt, tmp.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        let corrupt_tar = tmp.path().join("corrupt.tar.gz");
        fs::write(&corrupt_tar, b"not gzip").unwrap();
        assert!(extract_archive(&corrupt_tar, tmp.path()).is_err());

        let missing = tmp.path().join("missing.tar.gz");
        assert!(extract_archive(&missing, tmp.path()).is_err());
    }

    // ---------------------------------------------------------- install

    #[test]
    fn installs_the_cli_and_marks_it_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let extract = tmp.path().join("cli");
        fs::create_dir_all(&extract).unwrap();
        fs::write(extract.join(CLI_BIN), b"#!/bin/sh\n").unwrap();
        let dest = tmp.path().join("bin/aria-agent");
        fs::create_dir_all(dest.parent().unwrap()).unwrap();

        install_cli(&extract, &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"#!/bin/sh\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        // The staging file is cleaned up.
        let leftovers: Vec<_> = fs::read_dir(dest.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn install_cli_errors_when_the_binary_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let extract = tmp.path().join("cli");
        fs::create_dir_all(&extract).unwrap();
        let dest = tmp.path().join("aria-agent");
        let err = install_cli(&extract, &dest).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains(CLI_BIN));
    }

    #[test]
    fn install_cli_rejects_a_destination_without_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let extract = tmp.path().join("cli");
        fs::create_dir_all(&extract).unwrap();
        fs::write(extract.join(CLI_BIN), b"bin").unwrap();
        // `/` has no parent, so no staging directory can be derived from it.
        let err = install_cli(&extract, Path::new("/")).unwrap_err();
        assert!(err.to_string().contains("no parent"));
    }

    /// Cross-device rename: staging on the default filesystem, destination on
    /// `/dev/shm` (a tmpfs). `fs::rename` fails with `EXDEV` and the copy
    /// fallback takes over.
    #[cfg(target_os = "linux")]
    #[test]
    fn install_cli_falls_back_to_copy_across_filesystems() {
        // Linux always mounts a tmpfs at /dev/shm; if that ever changes the
        // test fails loudly rather than silently skipping.
        let shm = Path::new("/dev/shm");
        assert!(shm.is_dir(), "/dev/shm (tmpfs) is required for this test");
        let tmp = tempfile::tempdir().unwrap();
        let extract = tmp.path().join("cli");
        fs::create_dir_all(&extract).unwrap();
        fs::write(extract.join(CLI_BIN), b"binary").unwrap();

        let shm_dir = shm.join(format!("aria-upgrade-test-{}", std::process::id()));
        fs::create_dir_all(&shm_dir).unwrap();
        let dest = shm_dir.join("aria-agent");
        let res = install_cli_with_tmp(&extract, &dest, tmp.path());
        let cleanup = fs::remove_dir_all(&shm_dir);
        res.unwrap();
        cleanup.unwrap();
        // `dest` was removed with the directory; the copy itself is asserted
        // through the success of the operation above.
    }

    /// The fallback's error path: `rename` fails and the copy fails too (the
    /// destination is a directory), so the original rename error is returned.
    #[test]
    fn install_cli_reports_the_rename_error_when_copy_also_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let extract = tmp.path().join("cli");
        fs::create_dir_all(&extract).unwrap();
        fs::write(extract.join(CLI_BIN), b"binary").unwrap();
        let dest_dir = tmp.path().join("dest-is-a-directory");
        fs::create_dir_all(&dest_dir).unwrap();

        let err = install_cli_with_tmp(&extract, &dest_dir, tmp.path()).unwrap_err();
        // The staging file is always cleaned up.
        assert!(!tmp
            .path()
            .read_dir()
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains(".tmp")));
        assert!(err.raw_os_error().is_some() || err.kind() == io::ErrorKind::Other);
    }

    #[test]
    fn installs_known_and_versioned_ffi_libraries() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("lib");

        // Known names are copied verbatim.
        let extract = tmp.path().join("ffi-known");
        fs::create_dir_all(extract.join("nested")).unwrap();
        fs::write(extract.join("nested/libaria-agent_ffi.so"), b"so").unwrap();
        fs::write(extract.join("libaria-agent_ffi.a"), b"a").unwrap();
        install_ffi(&extract, &lib).unwrap();
        assert!(lib.join("libaria-agent_ffi.so").exists());
        assert!(lib.join("libaria-agent_ffi.a").exists());

        // Unknown-but-matching names are found by the fallback scan.
        let lib2 = tmp.path().join("lib2");
        let extract2 = tmp.path().join("ffi-versioned");
        fs::create_dir_all(&extract2).unwrap();
        fs::write(extract2.join("libaria-agent_ffi-0.9.1.so"), b"so").unwrap();
        install_ffi(&extract2, &lib2).unwrap();
        assert!(lib2.join("libaria-agent_ffi-0.9.1.so").exists());
    }

    #[test]
    fn install_ffi_errors_when_no_library_is_present() {
        let tmp = tempfile::tempdir().unwrap();
        let extract = tmp.path().join("empty");
        fs::create_dir_all(&extract).unwrap();
        fs::write(extract.join("README"), b"nothing").unwrap();
        let err = install_ffi(&extract, &tmp.path().join("lib")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("no libaria-agent_ffi"));
    }

    #[test]
    fn file_walking_and_lookup() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("a/b")).unwrap();
        fs::write(tmp.path().join("a/b/target.bin"), b"x").unwrap();
        assert_eq!(walk_files(tmp.path()).unwrap().len(), 1);
        assert_eq!(
            find_named_file(tmp.path(), "target.bin")
                .unwrap()
                .unwrap()
                .file_name()
                .unwrap(),
            "target.bin"
        );
        assert!(find_named_file(tmp.path(), "nope.bin").unwrap().is_none());
        assert!(walk_files(&tmp.path().join("missing")).is_err());
    }

    // --------------------------------------------------- HTTP round-trips

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// A release body with both assets pointing at `base`.
    fn release_json(base: &str, ver: &str, os: &str) -> serde_json::Value {
        serde_json::json!([{
            "tag_name": format!("v{ver}"),
            "draft": false,
            "prerelease": false,
            "assets": [
                {
                    "name": format!("aria-agent_{ver}_{os}.tar.gz"),
                    "browser_download_url": format!("{base}/dl/cli"),
                },
                {
                    "name": format!("libaria-agent_ffi_{ver}_{os}.tar.gz"),
                    "browser_download_url": format!("{base}/dl/ffi"),
                }
            ]
        }])
    }

    #[tokio::test]
    async fn full_upgrade_against_a_local_stub() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        let os = detect_asset_os().unwrap();

        // The CLI archive, built up front so the body is static bytes.
        let cli_dir = tmp.path().join("payload");
        fs::create_dir_all(&cli_dir).unwrap();
        let cli_tar = tar_gz(&cli_dir, "cli.tar.gz", &[("aria-agent", b"new-binary")]);
        let ffi_tar = tar_gz(
            &cli_dir,
            "ffi.tar.gz",
            &[("lib/libaria-agent_ffi.so", b"new-lib")],
        );
        let cli_bytes = fs::read(&cli_tar).unwrap();
        let ffi_bytes = fs::read(&ffi_tar).unwrap();

        Mock::given(method("GET"))
            .and(path("/repos/aci/agent/releases"))
            .respond_with(ResponseTemplate::new(200).set_body_json(release_json(
                &server.uri(),
                "9.9.9",
                os,
            )))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/dl/cli"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(cli_bytes))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/dl/ffi"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(ffi_bytes))
            .mount(&server)
            .await;

        let targets = InstallTargets {
            staging_root: tmp.path().join("home"),
            cli_dest: tmp.path().join("bin/aria-agent"),
            lib_dir: tmp.path().join("lib"),
        };
        fs::create_dir_all(targets.cli_dest.parent().unwrap()).unwrap();
        // Pre-create the staging directory: the upgrade must replace it.
        let staging = targets.staging_root.join("tmp/upgrade-9.9.9");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("stale"), b"old").unwrap();

        upgrade_into(
            &targets,
            "https://github.com/aci",
            Some(&server.uri()),
            None,
            "1.0.0",
        )
        .await
        .unwrap();

        assert_eq!(fs::read(&targets.cli_dest).unwrap(), b"new-binary");
        assert!(targets.lib_dir.join("libaria-agent_ffi.so").exists());
        assert!(!staging.exists(), "staging directory is cleaned up");
    }

    #[tokio::test]
    async fn upgrade_is_a_noop_at_the_current_version() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/aci/agent/releases"))
            .respond_with(ResponseTemplate::new(200).set_body_json(release_json(
                &server.uri(),
                "9.9.9",
                "linux_x86_64",
            )))
            .mount(&server)
            .await;
        let targets = InstallTargets {
            staging_root: tmp.path().join("home"),
            cli_dest: tmp.path().join("aria-agent"),
            lib_dir: tmp.path().join("lib"),
        };
        upgrade_into(
            &targets,
            "https://github.com/aci",
            Some(&server.uri()),
            Some("v9.9.9"),
            "9.9.9",
        )
        .await
        .unwrap();
        assert!(!targets.cli_dest.exists(), "nothing installed");
    }

    #[tokio::test]
    async fn missing_assets_and_download_failures_are_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        // Release without the FFI asset.
        Mock::given(method("GET"))
            .and(path("/repos/aci/agent/releases"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                    "tag_name": "v2.0.0",
                    "assets": [{
                        "name": format!("aria-agent_2.0.0_{}.tar.gz", detect_asset_os().unwrap()),
                        "browser_download_url": format!("{}/dl/cli", server.uri()),
                    }],
                }])),
            )
            .mount(&server)
            .await;
        let targets = InstallTargets {
            staging_root: tmp.path().join("home1"),
            cli_dest: tmp.path().join("bin/aria-agent"),
            lib_dir: tmp.path().join("lib"),
        };
        fs::create_dir_all(targets.cli_dest.parent().unwrap()).unwrap();
        let err = upgrade_into(
            &targets,
            "https://github.com/aci",
            Some(&server.uri()),
            None,
            "1.0.0",
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("libaria-agent_ffi"));
    }

    #[tokio::test]
    async fn a_staging_path_that_is_a_file_fails_the_upgrade() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/aci/agent/releases"))
            .respond_with(ResponseTemplate::new(200).set_body_json(release_json(
                &server.uri(),
                "3.0.0",
                detect_asset_os().unwrap(),
            )))
            .mount(&server)
            .await;
        let targets = InstallTargets {
            staging_root: tmp.path().join("home"),
            cli_dest: tmp.path().join("bin/aria-agent"),
            lib_dir: tmp.path().join("lib"),
        };
        fs::create_dir_all(targets.cli_dest.parent().unwrap()).unwrap();
        // The staging root is a *file*, so `tmp/upgrade-3.0.0` cannot be created
        // or removed as a directory: the upgrade must report the error instead
        // of continuing with a half-usable staging path.
        fs::write(&targets.staging_root, b"not a directory").unwrap();

        let err = upgrade_into(
            &targets,
            "https://github.com/aci",
            Some(&server.uri()),
            None,
            "1.0.0",
        )
        .await
        .unwrap_err();
        assert!(err.raw_os_error().is_some() || err.kind() != io::ErrorKind::NotFound);
        assert!(!targets.cli_dest.exists());
    }

    #[tokio::test]
    async fn releases_api_errors_and_bad_payloads() {
        let tmp = tempfile::tempdir().unwrap();
        let targets = InstallTargets {
            staging_root: tmp.path().join("home"),
            cli_dest: tmp.path().join("aria-agent"),
            lib_dir: tmp.path().join("lib"),
        };

        // Non-2xx.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/aci/agent/releases"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let err = upgrade_into(
            &targets,
            "https://github.com/aci",
            Some(&server.uri()),
            None,
            "1.0.0",
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("releases API"));

        // 200 with a non-JSON body.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/aci/agent/releases"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        let err = upgrade_into(
            &targets,
            "https://github.com/aci",
            Some(&server.uri()),
            None,
            "1.0.0",
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
    }

    #[tokio::test]
    async fn download_http_error_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/dl"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let dest = tmp.path().join("out");
        let err = download_url(&format!("{}/dl", server.uri()), &dest, "thing")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("download failed 404"));
    }

    #[tokio::test]
    async fn download_writes_the_body_and_handles_an_empty_label() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/dl"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"payload".to_vec()))
            .mount(&server)
            .await;
        let dest = tmp.path().join("out");
        download_url(&format!("{}/dl", server.uri()), &dest, "")
            .await
            .unwrap();
        let mut got = Vec::new();
        fs::File::open(&dest)
            .unwrap()
            .read_to_end(&mut got)
            .unwrap();
        assert_eq!(got, b"payload");
    }

    #[tokio::test]
    async fn download_fails_on_an_unreachable_host() {
        let tmp = tempfile::tempdir().unwrap();
        let err = download_url(
            "http://127.0.0.1:0/nope",
            &tmp.path().join("out"),
            "unreachable",
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("error sending request")
                || err.to_string().contains("connect")
        );
    }

    #[tokio::test]
    async fn a_failing_cli_download_aborts_the_upgrade() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        let os = detect_asset_os().unwrap();
        Mock::given(method("GET"))
            .and(path("/repos/aci/agent/releases"))
            .respond_with(ResponseTemplate::new(200).set_body_json(release_json(
                &server.uri(),
                "4.0.0",
                os,
            )))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/dl/cli"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let targets = InstallTargets {
            staging_root: tmp.path().join("home"),
            cli_dest: tmp.path().join("bin/aria-agent"),
            lib_dir: tmp.path().join("lib"),
        };
        fs::create_dir_all(targets.cli_dest.parent().unwrap()).unwrap();
        let err = upgrade_into(
            &targets,
            "https://github.com/aci",
            Some(&server.uri()),
            None,
            "1.0.0",
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("download failed 500"), "{err}");
    }

    #[tokio::test]
    async fn a_failing_ffi_download_aborts_the_upgrade() {
        let tmp = tempfile::tempdir().unwrap();
        let server = MockServer::start().await;
        let os = detect_asset_os().unwrap();
        let cli_dir = tmp.path().join("payload");
        fs::create_dir_all(&cli_dir).unwrap();
        let cli_bytes = fs::read(tar_gz(
            &cli_dir,
            "cli.tar.gz",
            &[("aria-agent", b"new-binary")],
        ))
        .unwrap();
        Mock::given(method("GET"))
            .and(path("/repos/aci/agent/releases"))
            .respond_with(ResponseTemplate::new(200).set_body_json(release_json(
                &server.uri(),
                "5.0.0",
                os,
            )))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/dl/cli"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(cli_bytes))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/dl/ffi"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let targets = InstallTargets {
            staging_root: tmp.path().join("home"),
            cli_dest: tmp.path().join("bin/aria-agent"),
            lib_dir: tmp.path().join("lib"),
        };
        fs::create_dir_all(targets.cli_dest.parent().unwrap()).unwrap();
        let err = upgrade_into(
            &targets,
            "https://github.com/aci",
            Some(server.uri().as_str()),
            None,
            "1.0.0",
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("download failed 404"), "{err}");
        assert!(!targets.cli_dest.exists(), "nothing installed on failure");
    }
}
