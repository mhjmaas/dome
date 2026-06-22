use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use serde::Deserialize;
use tar::Archive;

const GITHUB_REPO: &str = "mhjmaas/dome";
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Path to the immutable, version-addressed rootfs base image. Each OS version is
/// stored under its own filename so an upgrade never overwrites a base image that a
/// sandbox is pinned to (which would silently corrupt its never-written chunks).
pub fn versioned_rootfs_path(data_dir: &str, version: &str) -> String {
    format!("{}/rootfs-{}.ext4", data_dir, version)
}

/// The currently installed OS image version, read from the `VERSION` file written on
/// the last successful download. `None` if no image has been installed yet.
pub fn installed_version(data_dir: &str) -> Option<String> {
    fs::read_to_string(format!("{}/VERSION", data_dir))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Map a tarball entry name to its on-disk destination, or `None` to skip it. The
/// rootfs is stored under an immutable, versioned filename; the kernel and initramfs
/// are loaded fresh into VM memory each boot (never a CAS fallback) so they live at
/// fixed, version-agnostic paths and may be replaced in place on upgrade.
fn asset_dest(data_dir: &str, version: &str, entry_name: &str) -> Option<String> {
    match entry_name {
        "rootfs.ext4" => Some(versioned_rootfs_path(data_dir, version)),
        "Image" | "initramfs.cpio.gz" => Some(format!("{}/{}", data_dir, entry_name)),
        _ => None,
    }
}

/// Check if OS image assets exist and match the expected version.
pub fn assets_ready(data_dir: &str) -> bool {
    let version = match installed_version(data_dir) {
        Some(v) if v == CURRENT_VERSION => v,
        _ => return false,
    };
    let kernel = format!("{}/Image", data_dir);
    let initramfs = format!("{}/initramfs.cpio.gz", data_dir);
    let rootfs = versioned_rootfs_path(data_dir, &version);

    Path::new(&kernel).exists() && Path::new(&initramfs).exists() && Path::new(&rootfs).exists()
}

/// Download and extract OS image assets from GitHub Releases.
///
/// Streams directly: HTTP → gzip decompress → tar extract → disk.
/// No temp files needed.
pub fn download_os_image(data_dir: &str) -> Result<()> {
    download_os_image_version(data_dir, CURRENT_VERSION)
}

fn download_os_image_version(data_dir: &str, version: &str) -> Result<()> {
    let tag = format!("v{}", version);
    let tarball_name = format!("dome-os-{}-aarch64.tar.gz", tag);
    let url = format!(
        "https://github.com/{}/releases/download/{}/{}",
        GITHUB_REPO, tag, tarball_name
    );

    fs::create_dir_all(data_dir)
        .with_context(|| format!("failed to create data directory: {}", data_dir))?;

    eprintln!("dome: downloading OS image ({})...", tag);
    eprintln!("dome: {}", url);

    let response = ureq::get(&url)
        .call()
        .with_context(|| format!("download failed — is version {} released?", tag))?;

    let total_bytes = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    let reader = ProgressReader::new(response.into_body().into_reader(), total_bytes);
    let decoder = GzDecoder::new(reader);
    let mut archive = Archive::new(decoder);

    // Extract each entry to its versioned/fixed destination. The rootfs lands under
    // an immutable `rootfs-<version>.ext4` filename, so downloading a new version is
    // non-destructive: it never overwrites or deletes a prior version's base image
    // that an existing sandbox is still pinned to.
    for entry in archive
        .entries()
        .context("failed to read OS image archive")?
    {
        let mut entry = entry.context("failed to read archive entry")?;
        let name = entry
            .path()
            .context("invalid archive entry path")?
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string());
        let Some(name) = name else { continue };
        let Some(dest) = asset_dest(data_dir, version, &name) else {
            continue;
        };
        let mut out =
            fs::File::create(&dest).with_context(|| format!("failed to create {}", dest))?;
        io::copy(&mut entry, &mut out).with_context(|| format!("failed to extract {}", dest))?;
    }

    eprintln!(); // newline after progress

    // Write VERSION last so a partially-extracted image is never seen as ready.
    let version_file = format!("{}/VERSION", data_dir);
    fs::write(&version_file, format!("{}\n", version)).context("failed to write VERSION file")?;

    eprintln!("dome: OS image ready ({})", version);
    Ok(())
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
}

fn cli_tarball_name(version: &str) -> Result<String> {
    let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-aarch64",
        ("linux", "aarch64") => "linux-aarch64",
        (os, arch) => bail!("automatic upgrade is not supported on {}-{}", os, arch),
    };

    Ok(format!("dome-v{}-{}.tar.gz", version, platform))
}

/// Check for a newer release and upgrade the CLI binary + OS image.
pub fn upgrade(data_dir: &str) -> Result<()> {
    if let Ok(exe) = std::env::current_exe() {
        if exe.to_string_lossy().contains("/Cellar/") {
            bail!("This copy was installed via Homebrew. Please run `brew upgrade dome` instead");
        }
    }

    eprintln!("dome: checking for updates...");

    let api_url = format!(
        "https://api.github.com/repos/{}/releases/latest",
        GITHUB_REPO
    );

    let response = ureq::get(&api_url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "dome")
        .call()
        .context("failed to check for updates")?;

    let release: GithubRelease = response
        .into_body()
        .read_json()
        .context("failed to parse release info")?;

    let latest = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);

    if latest == CURRENT_VERSION {
        eprintln!("dome: already on latest version ({})", CURRENT_VERSION);
        return Ok(());
    }

    eprintln!("dome: upgrading {} -> {}", CURRENT_VERSION, latest);

    // Update CLI binary
    let cli_tarball = cli_tarball_name(latest)?;
    let cli_url = format!(
        "https://github.com/{}/releases/download/v{}/{}",
        GITHUB_REPO, latest, cli_tarball
    );

    let current_exe = std::env::current_exe().context("failed to determine current binary path")?;

    eprintln!("dome: downloading CLI ({})...", latest);
    eprintln!("dome: {}", cli_url);

    let response = ureq::get(&cli_url)
        .call()
        .with_context(|| format!("failed to download CLI v{}", latest))?;

    let total_bytes = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    let reader = ProgressReader::new(response.into_body().into_reader(), total_bytes);
    let decoder = GzDecoder::new(reader);
    let mut archive = Archive::new(decoder);

    // Extract to a temp file next to the current binary
    let tmp_path = current_exe.with_extension("new");
    for entry in archive.entries().context("failed to read CLI archive")? {
        let mut entry = entry.context("failed to read archive entry")?;
        if entry.path()?.to_str() == Some("dome") {
            let mut out = fs::File::create(&tmp_path).context("failed to create temp binary")?;
            io::copy(&mut entry, &mut out)?;
            break;
        }
    }

    eprintln!();

    if !tmp_path.exists() {
        bail!("'dome' binary not found in CLI archive");
    }

    // Set executable permission
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o755))?;
    }

    // Atomic-ish replace: rename current -> .old, rename new -> current, remove .old
    let old_path = current_exe.with_extension("old");
    let _ = fs::remove_file(&old_path);
    fs::rename(&current_exe, &old_path)
        .context("failed to move current binary (try with sudo?)")?;
    if let Err(e) = fs::rename(&tmp_path, &current_exe) {
        // Rollback
        let _ = fs::rename(&old_path, &current_exe);
        return Err(e).context("failed to install new binary");
    }
    let _ = fs::remove_file(&old_path);

    eprintln!("dome: CLI updated to {}", latest);

    // Update OS image
    download_os_image_version(data_dir, latest)?;

    eprintln!("dome: upgrade complete ({})", latest);
    Ok(())
}

/// Wraps a reader to print download progress to stderr.
struct ProgressReader<R> {
    inner: R,
    bytes_read: u64,
    total_bytes: Option<u64>,
    last_printed_mb: u64,
}

impl<R> ProgressReader<R> {
    fn new(inner: R, total_bytes: Option<u64>) -> Self {
        Self {
            inner,
            bytes_read: 0,
            total_bytes,
            last_printed_mb: u64::MAX, // force first print
        }
    }
}

impl<R: Read> Read for ProgressReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.bytes_read += n as u64;

        let current_mb = self.bytes_read / (1024 * 1024);
        if current_mb != self.last_printed_mb {
            self.last_printed_mb = current_mb;
            let mut stderr = io::stderr().lock();
            if let Some(total) = self.total_bytes {
                let total_mb = total / (1024 * 1024);
                let _ = write!(
                    stderr,
                    "\rdome: downloaded {} / {} MB",
                    current_mb, total_mb
                );
            } else {
                let _ = write!(stderr, "\rdome: downloaded {} MB", current_mb);
            }
            let _ = stderr.flush();
        }

        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rootfs_entry_maps_to_versioned_path() {
        assert_eq!(
            asset_dest("/data", "1.2.3", "rootfs.ext4"),
            Some("/data/rootfs-1.2.3.ext4".to_string())
        );
        assert_eq!(
            versioned_rootfs_path("/data", "1.2.3"),
            "/data/rootfs-1.2.3.ext4"
        );
    }

    #[test]
    fn kernel_and_initramfs_map_to_fixed_paths() {
        assert_eq!(
            asset_dest("/data", "1.2.3", "Image"),
            Some("/data/Image".to_string())
        );
        assert_eq!(
            asset_dest("/data", "1.2.3", "initramfs.cpio.gz"),
            Some("/data/initramfs.cpio.gz".to_string())
        );
    }

    #[test]
    fn unknown_entries_are_skipped() {
        assert_eq!(asset_dest("/data", "1.2.3", "README.md"), None);
    }

    #[test]
    fn different_versions_resolve_to_distinct_rootfs_files() {
        // The core non-destructive-upgrade property: each version owns its own base
        // file, so extracting a newer version can never target an older one's path.
        let v1 = asset_dest("/data", "1.0.0", "rootfs.ext4").unwrap();
        let v2 = asset_dest("/data", "2.0.0", "rootfs.ext4").unwrap();
        assert_ne!(v1, v2);
    }

    #[test]
    fn extracting_a_new_version_leaves_the_old_base_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();

        // Simulate an already-installed v1 base image.
        let v1 = asset_dest(data_dir, "1.0.0", "rootfs.ext4").unwrap();
        fs::write(&v1, b"v1-base-bytes").unwrap();

        // "Download" v2 by writing to its destination, exactly as the extract loop does.
        let v2 = asset_dest(data_dir, "2.0.0", "rootfs.ext4").unwrap();
        fs::write(&v2, b"v2-base-bytes").unwrap();

        // The older base a sandbox could be pinned to is untouched and both coexist.
        assert_eq!(fs::read(&v1).unwrap(), b"v1-base-bytes");
        assert_eq!(fs::read(&v2).unwrap(), b"v2-base-bytes");
    }

    #[test]
    fn assets_not_ready_without_version_file() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!assets_ready(tmp.path().to_str().unwrap()));
    }

    #[test]
    fn assets_ready_requires_the_versioned_rootfs_for_the_installed_version() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_str().unwrap();
        // Pretend the installed version matches this build.
        fs::write(
            format!("{}/VERSION", data_dir),
            format!("{}\n", CURRENT_VERSION),
        )
        .unwrap();
        fs::write(format!("{}/Image", data_dir), b"k").unwrap();
        fs::write(format!("{}/initramfs.cpio.gz", data_dir), b"i").unwrap();
        // No versioned rootfs yet → not ready.
        assert!(!assets_ready(data_dir));
        // Add the versioned rootfs → ready.
        fs::write(versioned_rootfs_path(data_dir, CURRENT_VERSION), b"r").unwrap();
        assert!(assets_ready(data_dir));
    }
}
