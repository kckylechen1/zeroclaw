# Windows

Install, update, run as a Windows scheduled task, and uninstall on Windows 10 / 11.

If you're running WSL2, you can follow the [Linux setup](./linux.md) instead; `install.sh` runs unchanged under WSL.

## Install from this fork

This fork installs from its source checkout. `setup.bat` defaults to the
recommended `dist` source preset. It checks that the checkout's `origin` is
`kckylechen1/zeroclaw` before installing a toolchain or building. Existing
checkouts with a different origin must be kept separate; clone this fork into
a new directory.

Windows source builds require:

- Git;
- Visual Studio Build Tools (or full Visual Studio) with the "Desktop
  development with C++" workload;
- Rust through `rustup`, using the repository's `rust-toolchain.toml`;
- Node.js and npm to build the web dashboard.

`setup.bat` can bootstrap Rust if it is missing. Install the Visual Studio
workload before running a source build. When npm is unavailable, the script
reports that it cannot install the dashboard.

From a Command Prompt:

```cmd
git clone https://github.com/kckylechen1/zeroclaw.git
cd zeroclaw
setup.bat --dry-run
setup.bat
zeroclaw --version
zeroclaw quickstart
```

The dry run verifies the source checkout and previews commands. It does not
build, download an application binary, install files, or change `PATH`.

| Flag | Behaviour |
|---|---|
| `--source` | Explicit source installation; also the default |
| `--dist` | Build the recommended lean distribution |
| `--minimal` | Build core only with `--no-default-features`; excludes Quickstart |
| `--default` | Build Cargo's default feature set |
| `--all` | Build the broad optional feature set |
| `--dry-run` | Preview the selected source build and installation |
| `--prebuilt` | Exit with source instructions before prerequisite checks or downloads |

The fork has no verified prebuilt binary channel. Upstream release archives,
and Scoop packages install the upstream product.
For a Linux environment on Windows, use a separate fork checkout inside WSL
and the source installer described in the [README](https://github.com/kckylechen1/zeroclaw/blob/master/README.md#install).

## Running as a service

On Windows, ZeroClaw installs as a **user-scoped scheduled task** named `ZeroClaw Daemon`. There is no Windows Service / LocalSystem option in the current release; the underlying code path always installs a scheduled task, regardless of whether `zeroclaw service install` is run from an elevated or non-elevated shell.

```cmd
zeroclaw service install
zeroclaw service start
```

This creates a task in Task Scheduler (`taskschd.msc`) under your user account that starts on login. Manage it via:

```cmd
zeroclaw service status
zeroclaw service restart
zeroclaw service stop
zeroclaw service logs
```

> **About `--service-init`.** The CLI exposes a `--service-init [auto|systemd|openrc]` flag for cross-platform consistency, but on Windows it is a no-op; the scheduled-task path is always used.

Logs go to `%USERPROFILE%\.zeroclaw\logs\` (specifically, `<config_dir>/logs/` where `<config_dir>` defaults to `%USERPROFILE%\.zeroclaw\`). The scheduled-task wrapper itself, however, lives next to the config file at `%USERPROFILE%\.zeroclaw\zeroclaw-daemon.cmd`. Only the daemon output files (`daemon.stdout.log` / `daemon.stderr.log`) are written under `logs\`.

> **Server / multi-user installs.** Native Windows Service / LocalSystem support is on the roadmap but not yet implemented. For now, on a server box, install ZeroClaw under the account that the agent should run as; the scheduled-task path will start it on that user's login. If you need it to start before any user logs in, use **Task Scheduler → ZeroClaw Daemon → Properties → General → "Run whether user is logged on or not."**

## Update

Stop the running daemon before replacing its executable. From the fork
checkout, confirm the configured origin is `kckylechen1/zeroclaw`, then
fast-forward and repeat the source preset used for the original installation:

```cmd
zeroclaw service stop
cd C:\path\to\zeroclaw
git remote get-url origin
git pull --ff-only origin master
setup.bat --dist
zeroclaw service start
```

If the pull cannot fast-forward, resolve the local branch or working-tree
changes before rerunning setup. The installer does not reset a checkout or
rewrite its origin. `zeroclaw update` and `setup.bat --prebuilt` return source
instructions; neither installs an upstream release.

## Uninstall

Stop and remove the scheduled task:

```cmd
zeroclaw service stop
zeroclaw service uninstall
```

Remove the binary:

```cmd
:: setup.bat installs here
rmdir /s /q "%USERPROFILE%\.zeroclaw\bin"

:: If you installed separately with cargo install
del "%USERPROFILE%\.cargo\bin\zeroclaw.exe"
```

Remove config, workspace, and logs (optional; this deletes conversation history):

```cmd
rmdir /s /q "%USERPROFILE%\.zeroclaw"
```

> The previous version of this doc referenced `%LOCALAPPDATA%\ZeroClaw\`; that path is **not** used by the current release; only `%USERPROFILE%\.zeroclaw\` is.

## Gotchas

- **Long paths.** Some Windows file systems still cap path lengths at 260 characters. Enable long path support if you hit `path too long` errors during a source build:
  ```
  reg add HKLM\SYSTEM\CurrentControlSet\Control\FileSystem /v LongPathsEnabled /t REG_DWORD /d 1 /f
  ```

- **SmartScreen.** The unsigned binary may trip SmartScreen on first launch from Explorer (double-click). Right-click → Properties → "Unblock" is the standard workaround until we add a signed MSI. Launching from PowerShell or `cmd.exe` typically does not trigger SmartScreen.

- **Task Scheduler stop-at-idle / battery.** By default Windows may terminate scheduled tasks on idle or battery. The installed `ZeroClaw Daemon` task disables these conditions, but if you've installed via an older release you can verify under **Task Scheduler → ZeroClaw Daemon → Properties → Conditions**:
  - "Start the task only if the computer is on AC power": unchecked
  - "Stop if the computer switches to battery power": unchecked
  - "Start the task only if the computer is idle for…": unchecked

- **OpenSSH password auth.** If you're driving Windows over SSH and pubkey isn't accepted, drop your key into `C:\Users\<user>\.ssh\authorized_keys` (regular user) or `C:\ProgramData\ssh\administrators_authorized_keys` (when logged in as a member of `Administrators`).

## Next

- [Service management](./service.md)
- [Quick start](../getting-started/quickstart.md)
- [Operations → Overview](../ops/overview.md)
