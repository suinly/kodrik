# Windows installation and source builds

Native Windows support includes x64 and ARM64 builds.
It is a Windows executable, not a WSL wrapper. The initial build target is
`x86_64-pc-windows-msvc`; Windows service installation is not part of the
initial port. Both x64 and ARM64 packages are built by GitHub Actions.

## Install a ready-made Windows ZIP

### PowerShell installer (recommended)

Download the installer, inspect it, then run it as your normal user:

```powershell
Invoke-WebRequest -UseBasicParsing https://raw.githubusercontent.com/suinly/kodrik/main/scripts/install-windows.ps1 -OutFile install-windows.ps1
notepad .\install-windows.ps1
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\install-windows.ps1 -AddToPath
```

`ExecutionPolicy Bypass` applies only to that process; it does not change the
machine policy. Organization policy or Windows security may still block an
unsigned script; do not disable those protections globally.

The installer selects the OS architecture (x64/ARM64), downloads the latest
stable release and verifies its SHA256 before running or installing it into
`%LOCALAPPDATA%\Programs\Kodrik`. SHA256 detects corruption, not publisher identity;
the script and hashes are trusted through the GitHub HTTPS source. No Rust,
Node.js, Visual Studio or administrator privileges are needed.

On first installation, the script asks for a model identifier, a hidden API key
and an optional API base URL (Enter uses OpenAI). The provider must support the
Responses API and tool calling. You can also configure Telegram by entering a
hidden bot token and your positive numeric Telegram user ID. No API test request
is sent; credentials are saved locally in UTF-8 TOML with a current-user-only
protected ACL at `$HOME\.kodrik\config.toml`. Existing configs are kept without
prompts or replacement. Use `-SkipConfigure` for unattended binary-only installs.

Use `-Version 0.5.0` to pin a release, `-InstallDir 'C:\your\folder'` for another
directory, and omit `-AddToPath` to leave PATH unchanged. Re-run to update;
stop Kodrik first if its executable is locked. Configuration/data are preserved;
no service, scheduled task or autostart is installed. Open a new terminal for
the updated user PATH. The manual configuration instructions below are only
needed if you skipped setup or want to change settings later.

### Manual installation

No Rust, Node.js, Visual Studio or source checkout is required. The executable
includes the web frontend and built-in skills. Run the following commands in
PowerShell as your normal user; administrator privileges are not required.

### 1. Choose and download a package

Open **Settings → System → About → System type** in Windows:

- For an Intel or AMD x64 processor, choose `x86_64-pc-windows-msvc`.
- For an ARM64 processor, choose `aarch64-pc-windows-msvc`.

On the [releases page](https://github.com/suinly/kodrik/releases), choose a
release whose assets include Windows packages. Download
`kodrik-<version>-<target>.zip` for your target and the matching `SHA256SUMS`
from that same release. Do not use GitHub's **Source code** ZIP.

If no published release has the Windows package you need, use
[Actions → Windows](https://github.com/suinly/kodrik/actions/workflows/windows.yml):
open a successful run for the branch/commit you intend to use and download
`kodrik-windows-x86_64` or `kodrik-windows-arm64` under **Artifacts**.
GitHub requires sign-in to download workflow artifacts. Extract the downloaded
artifact into a new folder: it contains the actual versioned program ZIP and
its `SHA256SUMS`. An artifact is a development build, not a published release.

### 2. Verify and extract

In PowerShell, change to the folder containing the **versioned program ZIP**
and `SHA256SUMS`. Set `$archive` to the actual downloaded filename, for example:

```powershell
Set-Location "$HOME\Downloads"
$archive = 'kodrik-<version>-x86_64-pc-windows-msvc.zip'
Get-FileHash -Algorithm SHA256 -LiteralPath $archive
Get-Content .\SHA256SUMS
```

Replace `<version>` (and the target for ARM64) before running the commands.
Compare the hash with the `SHA256SUMS` entry for that exact filename; letter
case does not matter. If they differ, stop and download the files again.
The checksum detects corruption; it is not a code signature.

Extract to a user-writable installation folder, then check that it runs:

```powershell
$installDir = Join-Path $env:LOCALAPPDATA 'Programs\Kodrik'
New-Item -ItemType Directory -Force -Path $installDir | Out-Null
Expand-Archive -LiteralPath $archive -DestinationPath $installDir
& "$installDir\kodrik.exe" --version
& "$installDir\kodrik.exe" --help
```

These steps assume a new installation directory. For an existing installation,
follow **Update** below. The executable is not code-signed, so Windows
SmartScreen may warn about it. Verify the source and checksum before deciding
whether to allow it; do not disable Windows security globally. If Windows reports
a missing Visual C++ runtime DLL, install Microsoft's
[Visual C++ v14 Redistributable](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist)
for your architecture and retry.

### 3. Configure the model

You need an API key and a model endpoint supporting the **OpenAI Responses API
and tool calling**; Chat Completions-only endpoints are insufficient.
Create the state directory and open the configuration file:

```powershell
New-Item -ItemType Directory -Force -Path "$HOME\.kodrik" | Out-Null
notepad "$HOME\.kodrik\config.toml"
```

For a new configuration, paste the following, replace the placeholders and save.
If the file already exists, edit it instead of replacing your settings.
Make sure Notepad saves `config.toml`, not `config.toml.txt`.

```toml
[gateway]

[provider.primary]
type = "openai"
model = "your-model"
api_key = "your-key"
# For a custom provider, uncomment and change:
# api_base = "https://your-provider.example/v1"
```

The empty `[gateway]` table is required even for terminal-only use. Keep this
file private: it contains your API key. Configuration and saved data live in
`$HOME\.kodrik`, separately from the installation folder. See
[Configuration](configuration.md) for more options.

### 4. Start Kodrik

Test a single question, or launch the terminal UI:

```powershell
& "$installDir\kodrik.exe" ask "Hello!"
& "$installDir\kodrik.exe"
```

The agent's file and shell tools work from the current directory, so change to
your intended working folder first. You do not need to add Kodrik to `PATH`.
In a new PowerShell window, set `$installDir` again as above or use the full
path to `kodrik.exe`.

For browser chat, add this to `config.toml`:

```toml
[web]
enabled = true
listen = "127.0.0.1:8080"
base_url = "http://127.0.0.1:8080/"
```

Then set the owner password and start the gateway:

```powershell
& "$installDir\kodrik.exe" web set-password
& "$installDir\kodrik.exe" gateway
```

Open <http://127.0.0.1:8080/app/chat> and sign in with that password.
Keep the terminal open while using the browser; Ctrl+C stops the gateway.
This binds only to your computer. Do not expose it to the network without
following the [HTTPS and access configuration](web-pages.md).
For Telegram, see [Telegram configuration](configuration.md#telegram).
Installing the ZIP does not install a background Windows service or enable
automatic startup.

### Update

Stop the gateway with Ctrl+C and close other running Kodrik instances.
Download the new package for the same architecture and verify its checksum as
above. Back up `$HOME\.kodrik` before upgrading; it contains private settings
and saved data. In the folder containing the new program ZIP, set `$archive`
to its filename and `$installDir` to your existing installation directory, then:

```powershell
Expand-Archive -LiteralPath $archive -DestinationPath $installDir -Force
& "$installDir\kodrik.exe" --version
```

Start Kodrik again using the commands above. Replacing files in the installation
folder does not replace your separate `$HOME\.kodrik\config.toml`.

## Build from source

Install the current stable Rust MSVC toolchain, Visual Studio Build Tools with
the **Desktop development with C++** workload and Windows SDK, and Node.js 24.15+
with npm. In PowerShell, from the repository root:

```powershell
npm ci --prefix web
npm run build --prefix web
cargo test --no-run --locked --target x86_64-pc-windows-msvc
cargo test --locked --target x86_64-pc-windows-msvc --lib platform::
cargo test --locked --target x86_64-pc-windows-msvc --lib windows_tests
cargo build --release --locked --target x86_64-pc-windows-msvc
.\target\x86_64-pc-windows-msvc\release\kodrik.exe --version
.\target\x86_64-pc-windows-msvc\release\kodrik.exe --help
```

The web frontend and built-in skills are embedded in the executable. Use the
normal [configuration guide](configuration.md); data is stored in `.kodrik`
under the user's home directory. Start the gateway in a terminal with
`kodrik.exe gateway` and stop it with Ctrl+C.

### Windows ARM64 virtual machine

Install the ARM64 Rust MSVC toolchain and the Visual Studio C++ ARM64 build
tools, Windows SDK and the **C++ Clang Compiler for Windows** component.
Open Developer PowerShell for Visual Studio with the ARM64 environment.
Use the same frontend commands above, then build for the native target:

```powershell
rustup target add aarch64-pc-windows-msvc
$env:CC_aarch64_pc_windows_msvc = (Get-Command clang-cl.exe -ErrorAction Stop).Source
cargo build --release --locked --target aarch64-pc-windows-msvc
.\target\aarch64-pc-windows-msvc\release\kodrik.exe --version
cargo test --locked --target aarch64-pc-windows-msvc --lib platform::
cargo test --locked --target aarch64-pc-windows-msvc --lib windows_tests
```

An ARM64 source build and Windows tests have been verified by the user in a
Windows VM. Native CI validation is separate from that manual verification.

## Automated verification

The **Windows** workflow runs a matrix on `windows-2025` (x64) and
`windows-11-arm` (native ARM64). It configures MSVC and ARM64 Clang, builds the
frontend, checks security backports, compiles all Rust tests and runs
the platform filesystem, Windows process and restart-PID tests on a native
Windows runner, builds the release executable, checks `--version` and `--help`,
and uploads a ZIP containing `kodrik.exe` and the license plus `SHA256SUMS`.
This produces a CI artifact, not a published GitHub release. A successful
cross-check on macOS does not replace a successful native workflow run.

Tagged releases also run this workflow and include both Windows ZIPs alongside
the macOS archive, with a combined `SHA256SUMS`. All three platform builds must
succeed before the release draft is created.

Download `kodrik-windows-x86_64` or `kodrik-windows-arm64` from the successful
run's **Artifacts** section. Each contains a target-specific ZIP and its
`SHA256SUMS`. The workflow runs on pushes to `main` and `feat/windows-native`,
on pull requests, and manually via **Actions → Windows → Run workflow**.

Security patch files are checked out with LF via `.gitattributes`, including
on Windows; do not disable the security-patch postinstall check.

Shell commands use Windows PowerShell syntax, not POSIX shell syntax. Existing
skills and scripts that require macOS or Unix tools need platform-specific
alternatives. WSL remains an alternative for running the Linux environment.

## Current limitations

- Computed dataset views in native UI documents fail explicitly on Windows:
  the disposable worker's CPU, memory and output resource limits have not yet
  been ported. These security limits are not silently disabled.
- Legacy owner migration containing symbolic links requires manual migration.
- File contents are flushed before replacement, but Windows directory-entry
  crash durability is not equivalent to Unix directory `fsync`.
- The executable is not code-signed; SmartScreen may warn about manual downloads.
- The complete test suite still contains POSIX shell commands and Unix runtime
  assumptions. Native CI runs the focused Windows tests, not the complete suite;
  passing these checks is not a claim of full feature parity.
