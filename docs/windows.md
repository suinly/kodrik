# Native Windows build

Work on native Windows support is developed in `feat/windows-native`.
It is a Windows executable, not a WSL wrapper. The initial build target is
`x86_64-pc-windows-msvc`; Windows service installation is not part of the
initial port. ARM64 source-build instructions are below; ARM64 CI packaging
has not yet been configured.

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
tools. Use the same frontend commands above, then build for the native target:

```powershell
rustup target add aarch64-pc-windows-msvc
cargo build --release --locked --target aarch64-pc-windows-msvc
.\target\aarch64-pc-windows-msvc\release\kodrik.exe --version
cargo test --locked --target aarch64-pc-windows-msvc --lib platform::
cargo test --locked --target aarch64-pc-windows-msvc --lib windows_tests
```

These ARM64 commands need validation in a Windows VM; a successful x64
cross-check does not establish ARM64 runtime support.

## Automated verification

The **Windows** workflow builds the frontend, compiles all Rust tests and runs
the platform filesystem, Windows process and restart-PID tests on a native
Windows runner, builds the release executable, checks `--version` and `--help`,
and uploads a ZIP containing `kodrik.exe` and the license plus `SHA256SUMS`.
This produces a CI artifact, not a published GitHub release. A successful
cross-check on macOS does not replace a successful native workflow run.

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
