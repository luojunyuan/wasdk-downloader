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

Download the current process architecture's installer into the standard cache:

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
All download functions return the verified installer path; they do not run the
installer.

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

The default cache is `%LOCALAPPDATA%\\WindowsAppSDK\\<version>\\<arch>`, with
the system temporary directory used when `LOCALAPPDATA` is unavailable.
Existing files are reused only after their size, PE header, and
version-and-architecture-specific SHA-256 have been verified. Failed downloads
leave the destination untouched.
