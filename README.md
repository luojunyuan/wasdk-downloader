# wasdk-downloader

A small Windows-only Rust library for downloading and verifying the Windows App
SDK runtime installer. The latest supported stable release is 2.6.5; 2.4.0 is
also available. Both releases support x86, x64, and ARM64. HTTP requests use
[`wrest`](https://crates.io/crates/wrest); SHA-256 verification uses Windows
CNG. The 2.6.5 installer digests were calculated from Microsoft's downloads,
whose Authenticode signatures verified with Microsoft Corporation as signer.

The latest stable release listed by [Microsoft Learn](https://learn.microsoft.com/en-us/windows/apps/windows-app-sdk/downloads)
is 2.6.5 (November 2, 2026). The 2.6.6 release is experimental and is not
included.

## Use

Add the library to the consuming project's `Cargo.toml`:

```toml
[dependencies]
wasdk-downloader = { path = "../wasdk-downloader" }
```

Download the current process architecture's installer into the system
temporary directory:

```rust,no_run
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let installer = wasdk_downloader::download()?;
    println!("{}", installer.display());
    Ok(())
}
```

Check the default runtime before downloading its installer:

```rust,no_run
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if !wasdk_downloader::runtime_is_installed()? {
        let installer = wasdk_downloader::download()?;
        println!("{}", installer.display());
    }
    Ok(())
}
```

`runtime_is_installed` checks the default version for the current process
architecture. Use `runtime_version_is_installed(RuntimeVersion::V2_4_0)` to
check an application's lower runtime requirement; a newer version in the same
major line is considered compatible. These functions inspect the current
user's registered Framework package and do not run an installer.

To choose a path or architecture explicitly:

```rust,no_run
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::path::Path;
    use wasdk_downloader::Architecture;

    let installer = wasdk_downloader::download_for_to(
        Architecture::Arm64,
        Path::new("cache/windowsappruntimeinstall-arm64.exe"),
    )?;
    println!("{}", installer.display());
    Ok(())
}
```

Select a specific supported version:

```rust,no_run
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use wasdk_downloader::{Architecture, RuntimeVersion};

    let installer = wasdk_downloader::download_version_for_to(
        RuntimeVersion::V2_4_0,
        Architecture::X64,
        "cache/windowsappruntimeinstall-x64.exe",
    )?;
    println!("{}", installer.display());
    Ok(())
}
```

Async applications can use `download_async`, `download_to_async`, or
`download_for_to_async`. The blocking functions use `wrest`'s bundled executor.
Download functions return the verified installer path without running it.

## Runtime setup

Use `ensure_runtime_with_progress` when the application should install the
default runtime automatically. It checks for an existing compatible runtime,
downloads and verifies the installer if needed, runs it quietly, then verifies
that the runtime package is registered:

```rust,no_run
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use wasdk_downloader::{RuntimeSetupOutcome, RuntimeSetupProgress};

    let outcome = wasdk_downloader::ensure_runtime_with_progress(|progress| match progress {
        RuntimeSetupProgress::AlreadyInstalled => {}
        RuntimeSetupProgress::RuntimeMissing => println!("Runtime is missing"),
        RuntimeSetupProgress::Downloading(progress) => {
            if let Some(total) = progress.total_bytes {
                println!("Downloading: {} / {} bytes", progress.received_bytes, total);
            } else {
                println!("Downloading: {} bytes", progress.received_bytes);
            }
        }
        RuntimeSetupProgress::Installing => println!("Installing runtime"),
        RuntimeSetupProgress::Installed => println!("Runtime installed"),
    })?;

    if outcome == RuntimeSetupOutcome::AlreadyInstalled {
        println!("Runtime was already installed");
    }
    Ok(())
}
```

Installer progress is reported as a phase only; the Microsoft installer does
not expose a reliable percentage. Installer exit codes `3010` and `1641` are
accepted, followed by a package-registration check. Once installation is
verified, the library attempts to remove the installer executable from the
temporary directory. A cleanup failure is logged without treating the
successful installation as a failure. Install failures leave the installer in
place for diagnosis or retry.

Enable the optional `toast` feature to report setup through Windows toast
notifications instead of implementing a progress UI in the application:

```toml
wasdk-downloader = { path = "../wasdk-downloader", features = ["toast"] }
```

https://github.com/user-attachments/assets/1c3f6a7f-3d64-4307-80cc-27c47ae28bba

```rust,no_run
fn main() -> Result<(), Box<dyn std::error::Error>> {
    wasdk_downloader::ensure_runtime_with_toast(
        "Example.WindowsAppRuntime",
        "Example App",
    )?;
    Ok(())
}
```

The feature uses the `windows` crate from the Windows-rs revision pinned by
this project. Toast notifications are best-effort; runtime setup errors are
still returned to the caller. The feature temporarily registers the requested
AppUserModelId and restores its previous `DisplayName`, or removes the value it
added. It removes the AUMID key only when this call created it and it remains
empty after cleanup.

## Progress reporting

Use `download_with_progress` (or `download_async_with_progress`) to receive
byte-level updates for the default installer:

```rust,no_run
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let installer = wasdk_downloader::download_with_progress(|progress| {
        if let Some(total) = progress.total_bytes {
            println!("{} / {} bytes", progress.received_bytes, total);
        } else {
            println!("{} bytes", progress.received_bytes);
        }
    })?;
    println!("{}", installer.display());
    Ok(())
}
```

For a specific version, architecture, or destination, use
`download_version_for_to_with_progress`; the async equivalent is
`download_version_for_to_async_with_progress`. The callback receives an initial
zero-byte update and then one update after each response chunk is written.
`total_bytes` is `None` if the response has no usable content length. A valid
cached installer is returned without invoking the callback.

The default download path is `%TEMP%\\WindowsAppSDK\\<version>\\<arch>`. A
verified existing installer at that path is reused. Existing files are reused
only after their size, PE header, and version-and-architecture-specific SHA-256
have been verified. Failed downloads leave the destination untouched.
