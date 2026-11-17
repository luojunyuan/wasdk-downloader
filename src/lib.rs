//! Download and verify the Windows App SDK runtime installer.
//!
//! The latest supported release is Windows App SDK 2.6.5. Windows App SDK
//! 2.4.0 is also available by selecting [`RuntimeVersion::V2_4_0`]. The
//! blocking functions are convenient for simple applications; async
//! applications can call the corresponding `_async` functions.

#[cfg(not(windows))]
compile_error!("wasdk-downloader supports Windows targets only");

use std::ffi::c_void;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use wrest::Client;

/// The latest supported Windows App SDK release.
pub const RUNTIME_VERSION: &str = "2.6.5";

/// The release selected by the convenience [`download`] functions.
pub const DEFAULT_RUNTIME_VERSION: RuntimeVersion = RuntimeVersion::V2_6_5;

const V2_4_ARM64_URL: &str =
    "https://aka.ms/windowsappsdk/2.4/2.4.0/windowsappruntimeinstall-arm64.exe";
const V2_4_X64_URL: &str =
    "https://aka.ms/windowsappsdk/2.4/2.4.0/windowsappruntimeinstall-x64.exe";
const V2_4_X86_URL: &str =
    "https://aka.ms/windowsappsdk/2.4/2.4.0/windowsappruntimeinstall-x86.exe";
const V2_4_ARM64_SHA256: &str = "788665585dcbc2844e99483fda27809a91c2f36235b799b104d6649b68eb61b0";
const V2_4_X64_SHA256: &str = "851c35b0b0a59ce4c55f9171f601193322fc3413143b0dc3390ea11e14cfa7fc";
const V2_4_X86_SHA256: &str = "427c490230db95443d74c9b6e86c3272a85e8a5dc86408fb9da4c05050196f8f";

const V2_6_5_ARM64_URL: &str =
    "https://aka.ms/windowsappsdk/2.6/2.6.5/windowsappruntimeinstall-arm64.exe";
const V2_6_5_X64_URL: &str =
    "https://aka.ms/windowsappsdk/2.6/2.6.5/windowsappruntimeinstall-x64.exe";
const V2_6_5_X86_URL: &str =
    "https://aka.ms/windowsappsdk/2.6/2.6.5/windowsappruntimeinstall-x86.exe";
const V2_6_5_ARM64_SHA256: &str =
    "4cafe57b5e03384a39cca9c6c0dbbbf837cf76ac828e66238cf6160ed2575b13";
const V2_6_5_X64_SHA256: &str = "908e9a8ed8ad2ac765d6679543b0758812437e3ad6e3223f08d1730635cec8ec";
const V2_6_5_X86_SHA256: &str = "cb4fbfd1f4ebc29fa2003bab5eb32b52ef7fb20424faebc62fa53ba3e20eca31";

const MIN_INSTALLER_SIZE: u64 = 1_048_576;
const DOWNLOAD_BUFFER_SIZE: usize = 128 * 1024;
const FRAMEWORK_PACKAGE_NAME: &str = "Microsoft.WindowsAppRuntime.2";
const PACKAGE_PUBLISHER_ID: &str = "8wekyb3d8bbwe";
const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_INSUFFICIENT_BUFFER: i32 = 122;
const ERROR_NOT_FOUND: i32 = 1168;
const APPMODEL_ERROR_NO_PACKAGE: i32 = 15700;
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// A Windows App SDK runtime release with checked-in installer metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeVersion {
    /// Windows App SDK 2.4.0.
    V2_4_0,
    /// Windows App SDK 2.6.5, the current latest stable release.
    V2_6_5,
}

impl RuntimeVersion {
    /// Returns the semantic version string for this release.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::V2_4_0 => "2.4.0",
            Self::V2_6_5 => "2.6.5",
        }
    }

    const fn installer(self, architecture: Architecture) -> (&'static str, &'static str) {
        match (self, architecture) {
            (Self::V2_4_0, Architecture::X86) => (V2_4_X86_URL, V2_4_X86_SHA256),
            (Self::V2_4_0, Architecture::X64) => (V2_4_X64_URL, V2_4_X64_SHA256),
            (Self::V2_4_0, Architecture::Arm64) => (V2_4_ARM64_URL, V2_4_ARM64_SHA256),
            (Self::V2_6_5, Architecture::X86) => (V2_6_5_X86_URL, V2_6_5_X86_SHA256),
            (Self::V2_6_5, Architecture::X64) => (V2_6_5_X64_URL, V2_6_5_X64_SHA256),
            (Self::V2_6_5, Architecture::Arm64) => (V2_6_5_ARM64_URL, V2_6_5_ARM64_SHA256),
        }
    }

    const fn minimum_package_version(self) -> (u16, u16, u16, u16) {
        match self {
            Self::V2_4_0 => (2, 4, 0, 0),
            Self::V2_6_5 => (2, 6, 5, 0),
        }
    }
}

/// Windows processor architecture for which the runtime installer is built.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Architecture {
    /// 32-bit x86 Windows.
    X86,
    /// 64-bit x86 Windows.
    X64,
    /// 64-bit ARM Windows.
    Arm64,
}

impl Architecture {
    /// Returns the architecture of the process running this library.
    pub fn current() -> Result<Self, Error> {
        match std::env::consts::ARCH {
            "x86" => Ok(Self::X86),
            "x86_64" => Ok(Self::X64),
            "aarch64" => Ok(Self::Arm64),
            architecture => Err(Error::UnsupportedArchitecture(architecture.to_owned())),
        }
    }

    /// Returns the architecture name used by the Microsoft installer URL.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::X86 => "x86",
            Self::X64 => "x64",
            Self::Arm64 => "arm64",
        }
    }
}

/// Byte-level progress for an installer download.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DownloadProgress {
    /// Number of response-body bytes received so far.
    pub received_bytes: u64,
    /// Total response-body size when the server provides it.
    pub total_bytes: Option<u64>,
}

/// Errors that can occur while checking, downloading, or verifying the runtime.
#[derive(Debug)]
pub enum Error {
    /// The current process architecture is not supported by the runtime.
    UnsupportedArchitecture(String),
    /// The HTTP request failed.
    Http(wrest::Error),
    /// The server returned a non-success HTTP status.
    HttpStatus(u16),
    /// A local file operation failed.
    Io(io::Error),
    /// The response ended before the advertised number of bytes arrived.
    IncompleteDownload { expected: u64, received: u64 },
    /// The downloaded file failed the PE header, size, or SHA-256 checks.
    InvalidInstaller,
    /// Windows could not query the installed Windows App SDK packages.
    RuntimeQuery(i32),
    /// Windows returned an invalid package identity while querying the runtime.
    InvalidRuntimePackageIdentity,
    /// Windows could not create the executor used by a blocking call.
    Runtime(io::Error),
    /// Windows CNG failed while computing SHA-256.
    Hashing(i32),
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedArchitecture(architecture) => {
                write!(
                    formatter,
                    "unsupported Windows architecture: {architecture}"
                )
            }
            Self::Http(error) => write!(formatter, "HTTP request failed: {error}"),
            Self::HttpStatus(status) => write!(formatter, "download returned HTTP {status}"),
            Self::Io(error) => write!(formatter, "file operation failed: {error}"),
            Self::IncompleteDownload { expected, received } => write!(
                formatter,
                "download ended early: received {received} bytes, expected {expected}"
            ),
            Self::InvalidInstaller => write!(
                formatter,
                "Windows App SDK installer failed its size, PE header, or SHA-256 check"
            ),
            Self::RuntimeQuery(status) => write!(
                formatter,
                "could not query Windows App SDK packages (Win32 error {status})"
            ),
            Self::InvalidRuntimePackageIdentity => {
                write!(
                    formatter,
                    "Windows returned an invalid runtime package identity"
                )
            }
            Self::Runtime(error) => write!(formatter, "could not start HTTP executor: {error}"),
            Self::Hashing(status) => write!(
                formatter,
                "Windows CNG SHA-256 operation failed (NTSTATUS 0x{:08X})",
                *status as u32
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Http(error) => Some(error),
            Self::Io(error) | Self::Runtime(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Checks whether the default runtime version is registered for the current
/// process architecture and current Windows user.
pub fn runtime_is_installed() -> Result<bool, Error> {
    runtime_version_is_installed(DEFAULT_RUNTIME_VERSION)
}

/// Checks whether the default runtime version is registered for `architecture`
/// and the current Windows user.
pub fn runtime_is_installed_for(architecture: Architecture) -> Result<bool, Error> {
    runtime_version_is_installed_for(DEFAULT_RUNTIME_VERSION, architecture)
}

/// Checks whether `version` or a newer compatible runtime is registered for
/// the current process architecture and current Windows user.
pub fn runtime_version_is_installed(version: RuntimeVersion) -> Result<bool, Error> {
    runtime_version_is_installed_for(version, Architecture::current()?)
}

/// Checks whether `version` or a newer compatible runtime is registered for
/// `architecture` and the current Windows user.
pub fn runtime_version_is_installed_for(
    version: RuntimeVersion,
    architecture: Architecture,
) -> Result<bool, Error> {
    let family_name = wide(&format!("{FRAMEWORK_PACKAGE_NAME}_{PACKAGE_PUBLISHER_ID}"));
    let minimum_version = version.minimum_package_version();
    Ok(package_family_full_names(&family_name)?
        .iter()
        .any(|full_name| runtime_package_matches(full_name, architecture, minimum_version)))
}

/// Downloads the installer for the current architecture to the default cache path.
///
/// The cache is `%LOCALAPPDATA%\\WindowsAppSDK\\<version>\\<arch>`. A valid
/// installer already at that path is reused.
pub fn download() -> Result<PathBuf, Error> {
    download_version(DEFAULT_RUNTIME_VERSION)
}

/// Downloads the default installer and reports byte-level progress.
///
/// The callback is first called with zero bytes after the response headers are
/// received, then after each response chunk is written. `total_bytes` is
/// `None` when the response does not provide a usable content length. A valid
/// cached installer is reused without calling the callback.
pub fn download_with_progress<F>(on_progress: F) -> Result<PathBuf, Error>
where
    F: FnMut(DownloadProgress),
{
    let architecture = Architecture::current()?;
    download_version_for_to_with_progress(
        DEFAULT_RUNTIME_VERSION,
        architecture,
        default_path_for_version(DEFAULT_RUNTIME_VERSION, architecture),
        on_progress,
    )
}

/// Downloads the requested architecture's installer to the default cache path.
pub fn download_for(architecture: Architecture) -> Result<PathBuf, Error> {
    download_version_for(DEFAULT_RUNTIME_VERSION, architecture)
}

/// Downloads the current architecture's installer to `destination`.
pub fn download_to(destination: impl AsRef<Path>) -> Result<PathBuf, Error> {
    download_version_to(DEFAULT_RUNTIME_VERSION, destination)
}

/// Downloads the requested architecture's installer to `destination`.
///
/// A valid installer already at the destination is reused. New downloads are
/// written to a temporary sibling file and replace the destination only after
/// their size, PE header, and SHA-256 have been checked.
pub fn download_for_to(
    architecture: Architecture,
    destination: impl AsRef<Path>,
) -> Result<PathBuf, Error> {
    download_version_for_to(DEFAULT_RUNTIME_VERSION, architecture, destination)
}

/// Downloads the current architecture's installer for `version` to the default cache path.
pub fn download_version(version: RuntimeVersion) -> Result<PathBuf, Error> {
    download_version_for(version, Architecture::current()?)
}

/// Downloads the requested version and architecture to the default cache path.
pub fn download_version_for(
    version: RuntimeVersion,
    architecture: Architecture,
) -> Result<PathBuf, Error> {
    download_version_for_to(
        version,
        architecture,
        default_path_for_version(version, architecture),
    )
}

/// Downloads the current architecture's installer for `version` to `destination`.
pub fn download_version_to(
    version: RuntimeVersion,
    destination: impl AsRef<Path>,
) -> Result<PathBuf, Error> {
    download_version_for_to(version, Architecture::current()?, destination)
}

/// Downloads `version` for the requested architecture to `destination`.
pub fn download_version_for_to(
    version: RuntimeVersion,
    architecture: Architecture,
    destination: impl AsRef<Path>,
) -> Result<PathBuf, Error> {
    download_version_for_to_with_progress(version, architecture, destination, |_| {})
}

/// Downloads an installer to `destination` and reports byte-level progress.
///
/// The callback is first called with zero bytes after the response headers are
/// received, then after each response chunk is written. `total_bytes` is
/// `None` when the response does not provide a usable content length. A valid
/// installer already at the destination is reused without calling the callback.
pub fn download_version_for_to_with_progress<F>(
    version: RuntimeVersion,
    architecture: Architecture,
    destination: impl AsRef<Path>,
    on_progress: F,
) -> Result<PathBuf, Error>
where
    F: FnMut(DownloadProgress),
{
    let destination = destination.as_ref().to_path_buf();
    run_blocking(download_version_for_to_async_with_progress(
        version,
        architecture,
        destination,
        on_progress,
    ))
}

/// Asynchronously downloads the installer for the current architecture to the default cache path.
pub async fn download_async() -> Result<PathBuf, Error> {
    download_version_async(DEFAULT_RUNTIME_VERSION).await
}

/// Asynchronously downloads the default installer and reports byte-level progress.
///
/// The callback is first called with zero bytes after the response headers are
/// received, then after each response chunk is written. `total_bytes` is
/// `None` when the response does not provide a usable content length. A valid
/// cached installer is reused without calling the callback.
pub async fn download_async_with_progress<F>(on_progress: F) -> Result<PathBuf, Error>
where
    F: FnMut(DownloadProgress),
{
    let architecture = Architecture::current()?;
    download_version_for_to_async_with_progress(
        DEFAULT_RUNTIME_VERSION,
        architecture,
        default_path_for_version(DEFAULT_RUNTIME_VERSION, architecture),
        on_progress,
    )
    .await
}

/// Asynchronously downloads the requested architecture's installer to the default cache path.
pub async fn download_for_async(architecture: Architecture) -> Result<PathBuf, Error> {
    download_version_for_async(DEFAULT_RUNTIME_VERSION, architecture).await
}

/// Asynchronously downloads the current architecture's installer to `destination`.
pub async fn download_to_async(destination: impl AsRef<Path>) -> Result<PathBuf, Error> {
    download_version_to_async(DEFAULT_RUNTIME_VERSION, destination).await
}

/// Asynchronously downloads the requested architecture's installer to `destination`.
pub async fn download_for_to_async(
    architecture: Architecture,
    destination: impl AsRef<Path>,
) -> Result<PathBuf, Error> {
    download_version_for_to_async(DEFAULT_RUNTIME_VERSION, architecture, destination).await
}

/// Asynchronously downloads the current architecture's installer for `version` to the default cache path.
pub async fn download_version_async(version: RuntimeVersion) -> Result<PathBuf, Error> {
    download_version_for_async(version, Architecture::current()?).await
}

/// Asynchronously downloads the requested version and architecture to the default cache path.
pub async fn download_version_for_async(
    version: RuntimeVersion,
    architecture: Architecture,
) -> Result<PathBuf, Error> {
    download_version_for_to_async(
        version,
        architecture,
        default_path_for_version(version, architecture),
    )
    .await
}

/// Asynchronously downloads the current architecture's installer for `version` to `destination`.
pub async fn download_version_to_async(
    version: RuntimeVersion,
    destination: impl AsRef<Path>,
) -> Result<PathBuf, Error> {
    download_version_for_to_async(version, Architecture::current()?, destination).await
}

/// Asynchronously downloads `version` for the requested architecture to `destination`.
pub async fn download_version_for_to_async(
    version: RuntimeVersion,
    architecture: Architecture,
    destination: impl AsRef<Path>,
) -> Result<PathBuf, Error> {
    download_version_for_to_async_with_progress(version, architecture, destination, |_| {}).await
}

/// Asynchronously downloads an installer and reports byte-level progress.
///
/// The callback is first called with zero bytes after the response headers are
/// received, then after each response chunk is written. `total_bytes` is
/// `None` when the response does not provide a usable content length. A valid
/// installer already at the destination is reused without calling the callback.
pub async fn download_version_for_to_async_with_progress<F>(
    version: RuntimeVersion,
    architecture: Architecture,
    destination: impl AsRef<Path>,
    mut on_progress: F,
) -> Result<PathBuf, Error>
where
    F: FnMut(DownloadProgress),
{
    let destination = destination.as_ref().to_path_buf();
    let (url, expected_hash) = version.installer(architecture);
    let expected_hash = decode_sha256(expected_hash)?;

    if valid_installer(&destination, &expected_hash)? {
        return Ok(destination);
    }

    if let Some(parent) = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }

    let client = Client::builder()
        .user_agent(concat!("wasdk-downloader/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .build()
        .map_err(Error::Http)?;
    let mut response = client.get(url).send().await.map_err(Error::Http)?;
    let status = response.status();
    if !status.is_success() {
        return Err(Error::HttpStatus(status.as_u16()));
    }

    let expected_size = if response
        .headers()
        .contains_key(wrest::header::CONTENT_ENCODING)
    {
        None
    } else {
        response.content_length()
    };
    let (mut output, partial_path) = create_partial_file(&destination)?;
    let mut partial = PartialFile::new(partial_path);
    let mut received = 0_u64;
    on_progress(DownloadProgress {
        received_bytes: received,
        total_bytes: expected_size,
    });

    while let Some(chunk) = response.chunk().await.map_err(Error::Http)? {
        output.write_all(&chunk)?;
        received = received
            .checked_add(chunk.len() as u64)
            .ok_or(Error::InvalidInstaller)?;
        on_progress(DownloadProgress {
            received_bytes: received,
            total_bytes: expected_size,
        });
    }
    output.flush()?;
    drop(output);

    if let Some(expected) = expected_size
        && expected != received
    {
        return Err(Error::IncompleteDownload { expected, received });
    }
    if !valid_installer(partial.path(), &expected_hash)? {
        return Err(Error::InvalidInstaller);
    }

    if destination.exists() {
        fs::remove_file(&destination)?;
    }
    fs::rename(partial.path(), &destination)?;
    partial.keep();
    Ok(destination)
}

/// Returns the default cache location for the latest supported version.
pub fn default_path_for(architecture: Architecture) -> PathBuf {
    default_path_for_version(DEFAULT_RUNTIME_VERSION, architecture)
}

/// Returns the default cache location for a specific version and architecture.
pub fn default_path_for_version(version: RuntimeVersion, architecture: Architecture) -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("WindowsAppSDK")
        .join(version.as_str())
        .join(architecture.as_str())
        .join(format!(
            "WindowsAppRuntimeInstall-{}-{}.exe",
            version.as_str(),
            architecture.as_str(),
        ))
}

fn package_family_full_names(family_name: &[u16]) -> Result<Vec<String>, Error> {
    for _ in 0..3 {
        let mut count = 0_u32;
        let mut buffer_length = 0_u32;
        let status = unsafe {
            GetPackagesByPackageFamily(
                family_name.as_ptr(),
                &mut count,
                ptr::null_mut(),
                &mut buffer_length,
                ptr::null_mut(),
            )
        };
        if is_missing_package_status(status) {
            return Ok(Vec::new());
        }
        if status != 0 && status != ERROR_INSUFFICIENT_BUFFER {
            return Err(Error::RuntimeQuery(status));
        }
        if count == 0 {
            return Ok(Vec::new());
        }
        if buffer_length == 0 {
            return Err(Error::RuntimeQuery(ERROR_INSUFFICIENT_BUFFER));
        }

        let mut names = vec![ptr::null_mut(); count as usize];
        let mut buffer = vec![0_u16; buffer_length as usize];
        let status = unsafe {
            GetPackagesByPackageFamily(
                family_name.as_ptr(),
                &mut count,
                names.as_mut_ptr(),
                &mut buffer_length,
                buffer.as_mut_ptr(),
            )
        };
        if status == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        if is_missing_package_status(status) {
            return Ok(Vec::new());
        }
        if status != 0 {
            return Err(Error::RuntimeQuery(status));
        }
        if count as usize > names.len() {
            return Err(Error::InvalidRuntimePackageIdentity);
        }

        let buffer_start = buffer.as_ptr() as usize;
        let buffer_bytes = buffer.len() * std::mem::size_of::<u16>();
        let buffer_end = buffer_start
            .checked_add(buffer_bytes)
            .ok_or(Error::InvalidRuntimePackageIdentity)?;
        let mut full_names = Vec::with_capacity(count as usize);
        for name in names.into_iter().take(count as usize) {
            let address = name as usize;
            let offset_bytes = address
                .checked_sub(buffer_start)
                .ok_or(Error::InvalidRuntimePackageIdentity)?;
            if address >= buffer_end || offset_bytes % std::mem::size_of::<u16>() != 0 {
                return Err(Error::InvalidRuntimePackageIdentity);
            }
            let units = buffer
                .get(offset_bytes / std::mem::size_of::<u16>()..)
                .ok_or(Error::InvalidRuntimePackageIdentity)?;
            let length = units
                .iter()
                .position(|unit| *unit == 0)
                .ok_or(Error::InvalidRuntimePackageIdentity)?;
            let full_name = String::from_utf16(&units[..length])
                .map_err(|_| Error::InvalidRuntimePackageIdentity)?;
            full_names.push(full_name);
        }
        return Ok(full_names);
    }

    Err(Error::RuntimeQuery(ERROR_INSUFFICIENT_BUFFER))
}

fn is_missing_package_status(status: i32) -> bool {
    matches!(
        status,
        APPMODEL_ERROR_NO_PACKAGE | ERROR_FILE_NOT_FOUND | ERROR_NOT_FOUND
    )
}

fn runtime_package_matches(
    full_name: &str,
    architecture: Architecture,
    minimum_version: (u16, u16, u16, u16),
) -> bool {
    let Some(remainder) = full_name.strip_prefix(&format!("{FRAMEWORK_PACKAGE_NAME}_")) else {
        return false;
    };
    let mut components = remainder.split('_');
    let Some(installed_version) = components.next().and_then(parse_package_version) else {
        return false;
    };
    let Some(installed_architecture) = components.next() else {
        return false;
    };
    let Some(_resource_id) = components.next() else {
        return false;
    };
    let Some(publisher_id) = components.next() else {
        return false;
    };

    components.next().is_none()
        && publisher_id == PACKAGE_PUBLISHER_ID
        && installed_architecture == architecture.as_str()
        && installed_version.0 == minimum_version.0
        && installed_version >= minimum_version
}

fn parse_package_version(value: &str) -> Option<(u16, u16, u16, u16)> {
    let mut components = value.split('.');
    let version = (
        components.next()?.parse().ok()?,
        components.next()?.parse().ok()?,
        components.next()?.parse().ok()?,
        components.next()?.parse().ok()?,
    );
    components.next().is_none().then_some(version)
}

fn run_blocking(
    future: impl std::future::Future<Output = Result<PathBuf, Error>>,
) -> Result<PathBuf, Error> {
    wrest::block_on(future).map_err(Error::Runtime)?
}

fn decode_sha256(value: &str) -> Result<[u8; 32], Error> {
    if value.len() != 64 {
        return Err(Error::InvalidInstaller);
    }
    let mut digest = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_digit(pair[0]).ok_or(Error::InvalidInstaller)?;
        let low = hex_digit(pair[1]).ok_or(Error::InvalidInstaller)?;
        digest[index] = high << 4 | low;
    }
    Ok(digest)
}

fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn valid_installer(path: &Path, expected_hash: &[u8; 32]) -> Result<bool, Error> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(Error::Io(error)),
    };
    if !metadata.is_file() || metadata.len() <= MIN_INSTALLER_SIZE {
        return Ok(false);
    }

    let mut file = File::open(path)?;
    let mut header = [0_u8; 2];
    match file.read_exact(&mut header) {
        Ok(()) if header == *b"MZ" => {}
        Ok(()) => return Ok(false),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
        Err(error) => return Err(Error::Io(error)),
    }

    Ok(sha256_file(path)? == *expected_hash)
}

fn create_partial_file(destination: &Path) -> Result<(File, PathBuf), Error> {
    for _ in 0..16 {
        let mut candidate = destination.as_os_str().to_os_string();
        candidate.push(format!(
            ".wasdk-download-{}-{}",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let path = PathBuf::from(candidate);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(Error::Io(error)),
        }
    }
    Err(Error::Io(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary download file",
    )))
}

struct PartialFile {
    path: PathBuf,
    keep: bool,
}

impl PartialFile {
    fn new(path: PathBuf) -> Self {
        Self { path, keep: false }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn keep(&mut self) {
        self.keep = true;
    }
}

impl Drop for PartialFile {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetPackagesByPackageFamily(
        package_family_name: *const u16,
        count: *mut u32,
        package_full_names: *mut *mut u16,
        buffer_length: *mut u32,
        buffer: *mut u16,
    ) -> i32;
}

type BcryptHandle = *mut c_void;

#[link(name = "bcrypt")]
unsafe extern "system" {
    fn BCryptOpenAlgorithmProvider(
        algorithm: *mut BcryptHandle,
        algorithm_id: *const u16,
        implementation: *const u16,
        flags: u32,
    ) -> i32;
    fn BCryptGetProperty(
        object: BcryptHandle,
        property: *const u16,
        output: *mut u8,
        output_size: u32,
        result_size: *mut u32,
        flags: u32,
    ) -> i32;
    fn BCryptCreateHash(
        algorithm: BcryptHandle,
        hash: *mut BcryptHandle,
        hash_object: *mut u8,
        hash_object_size: u32,
        secret: *mut u8,
        secret_size: u32,
        flags: u32,
    ) -> i32;
    fn BCryptHashData(hash: BcryptHandle, input: *mut u8, input_size: u32, flags: u32) -> i32;
    fn BCryptFinishHash(hash: BcryptHandle, output: *mut u8, output_size: u32, flags: u32) -> i32;
    fn BCryptDestroyHash(hash: BcryptHandle) -> i32;
    fn BCryptCloseAlgorithmProvider(algorithm: BcryptHandle, flags: u32) -> i32;
}

struct AlgorithmHandle(NonNull<c_void>);

impl AlgorithmHandle {
    fn sha256() -> Result<Self, Error> {
        let algorithm_name = wide("SHA256");
        let mut handle = ptr::null_mut();
        let status = unsafe {
            BCryptOpenAlgorithmProvider(&mut handle, algorithm_name.as_ptr(), ptr::null(), 0)
        };
        check_ntstatus(status)?;
        NonNull::new(handle).map(Self).ok_or(Error::Hashing(-1))
    }

    fn property_u32(&self, property: &str) -> Result<u32, Error> {
        let property = wide(property);
        let mut value = 0_u32;
        let mut result_size = 0_u32;
        let status = unsafe {
            BCryptGetProperty(
                self.0.as_ptr(),
                property.as_ptr(),
                (&mut value as *mut u32).cast(),
                std::mem::size_of::<u32>() as u32,
                &mut result_size,
                0,
            )
        };
        check_ntstatus(status)?;
        if result_size != std::mem::size_of::<u32>() as u32 {
            return Err(Error::Hashing(-1));
        }
        Ok(value)
    }
}

impl Drop for AlgorithmHandle {
    fn drop(&mut self) {
        unsafe {
            BCryptCloseAlgorithmProvider(self.0.as_ptr(), 0);
        }
    }
}

struct HashHandle {
    handle: NonNull<c_void>,
    _object: Vec<u8>,
}

impl Drop for HashHandle {
    fn drop(&mut self) {
        unsafe {
            BCryptDestroyHash(self.handle.as_ptr());
        }
    }
}

fn sha256_file(path: &Path) -> Result<[u8; 32], Error> {
    let algorithm = AlgorithmHandle::sha256()?;
    let object_size = algorithm.property_u32("ObjectLength")?;
    let digest_size = algorithm.property_u32("HashDigestLength")?;
    if object_size == 0 || digest_size != 32 {
        return Err(Error::Hashing(-1));
    }

    let mut object = vec![0_u8; object_size as usize];
    let mut raw_hash = ptr::null_mut();
    let status = unsafe {
        BCryptCreateHash(
            algorithm.0.as_ptr(),
            &mut raw_hash,
            object.as_mut_ptr(),
            object_size,
            ptr::null_mut(),
            0,
            0,
        )
    };
    check_ntstatus(status)?;
    let handle = NonNull::new(raw_hash).ok_or(Error::Hashing(-1))?;
    let hash = HashHandle {
        handle,
        _object: object,
    };

    let mut file = File::open(path)?;
    let mut buffer = [0_u8; DOWNLOAD_BUFFER_SIZE];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let input_size = u32::try_from(read).map_err(|_| Error::Hashing(-1))?;
        let status =
            unsafe { BCryptHashData(hash.handle.as_ptr(), buffer.as_mut_ptr(), input_size, 0) };
        check_ntstatus(status)?;
    }

    let mut digest = [0_u8; 32];
    let status = unsafe {
        BCryptFinishHash(
            hash.handle.as_ptr(),
            digest.as_mut_ptr(),
            digest.len() as u32,
            0,
        )
    };
    check_ntstatus(status)?;
    Ok(digest)
}

fn check_ntstatus(status: i32) -> Result<(), Error> {
    if status >= 0 {
        Ok(())
    } else {
        Err(Error::Hashing(status))
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
