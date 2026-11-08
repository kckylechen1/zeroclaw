# Exercise the actual batch bootstrap; every scenario uses refusal or dry-run.
$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$installer = Join-Path $repoRoot 'setup.bat'
$realGit = (Get-Command git -CommandType Application | Select-Object -First 1).Source
$scratch = Join-Path ([System.IO.Path]::GetTempPath()) ('zeroclaw-installer-smoke-' + [guid]::NewGuid())
$originalPath = $env:PATH
$originalLog = $env:ZEROCLAW_SMOKE_LOG
$originalRealGit = $env:ZEROCLAW_REAL_GIT
$checks = 0

function Invoke-Git {
    param([string[]] $GitArguments)
    $result = & $realGit @GitArguments 2>&1
    if ($LASTEXITCODE -ne 0) { throw "Fixture git failed: $result" }
    return ($result -join "`n")
}

function New-Checkout {
    param([string] $Path, [string] $Origin)
    New-Item -ItemType Directory -Path $Path | Out-Null
    Copy-Item -LiteralPath (Join-Path $repoRoot 'Cargo.toml') -Destination $Path
    Set-Content -LiteralPath (Join-Path $Path 'local-marker') -Value 'preserve local edits'
    $null = Invoke-Git @('-C', $Path, 'init', '--quiet')
    $null = Invoke-Git @('-C', $Path, 'remote', 'add', 'origin', $Origin)
    $null = Invoke-Git @('-C', $Path, 'add', 'Cargo.toml', 'local-marker')
    $null = Invoke-Git @('-C', $Path, '-c', 'user.name=Installer fixture', '-c', 'user.email=installer@example.invalid', 'commit', '--quiet', '-m', 'fixture')
    Add-Content -LiteralPath (Join-Path $Path 'local-marker') -Value 'uncommitted local edit'
}

function Invoke-Setup {
    param([string] $Directory, [bool] $Success, [string[]] $Flags)
    $start = [System.Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $env:ComSpec
    $start.Arguments = '/d /s /c ""' + $installer + '" ' + ($Flags -join ' ') + '"'
    $start.WorkingDirectory = $Directory
    $start.UseShellExecute = $false
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $start
    try {
        if (-not $process.Start()) { throw 'Could not start setup.bat' }
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit(30000)) {
            $process.Kill($true)
            throw 'setup.bat hung or attempted an interactive install'
        }
        $output = $stdout.GetAwaiter().GetResult() + $stderr.GetAwaiter().GetResult()
        if (($process.ExitCode -eq 0) -ne $Success) {
            throw "Unexpected setup.bat exit $($process.ExitCode): $output"
        }
        if ((Get-Item -LiteralPath $env:ZEROCLAW_SMOKE_LOG).Length -ne 0) {
            throw "Unexpected mutation: $(Get-Content -Raw -LiteralPath $env:ZEROCLAW_SMOKE_LOG)"
        }
        $script:checks++
        return $output
    } finally {
        $process.Dispose()
    }
}

function Assert-Contains {
    param([string] $Output, [string] $Text)
    if (-not $Output.Contains($Text)) { throw "Missing '$Text': $Output" }
}

try {
    New-Item -ItemType Directory -Path $scratch | Out-Null
    $shim = Join-Path $scratch 'bin'
    $outside = Join-Path $scratch 'outside'
    New-Item -ItemType Directory -Path $shim, $outside | Out-Null
    $env:ZEROCLAW_SMOKE_LOG = Join-Path $scratch 'mutations.log'
    $env:ZEROCLAW_REAL_GIT = $realGit
    New-Item -ItemType File -Path $env:ZEROCLAW_SMOKE_LOG | Out-Null
    Set-Content -Encoding ascii -LiteralPath (Join-Path $shim 'git.cmd') -Value @'
@echo off
setlocal enabledelayedexpansion
if "%~1"=="remote" if "%~2"=="get-url" if "%~3"=="origin" (
    "%ZEROCLAW_REAL_GIT%" remote get-url origin
    exit /b !errorlevel!
)
echo git %*>>"%ZEROCLAW_SMOKE_LOG%"
exit /b 97
'@
    foreach ($name in @('cargo', 'curl', 'rustup', 'gh', 'tar', 'npm', 'setx')) {
        Set-Content -Encoding ascii -LiteralPath (Join-Path $shim "$name.cmd") -Value @'
@echo off
echo %~n0 %*>>"%ZEROCLAW_SMOKE_LOG%"
exit /b 97
'@
    }
    $fork = Join-Path $scratch 'fork'
    $upstream = Join-Path $scratch 'upstream'
    New-Checkout $fork 'https://github.com/kckylechen1/zeroclaw.git'
    New-Checkout $upstream 'https://github.com/zeroclaw-labs/zeroclaw.git'
    $upstreamHead = Invoke-Git @('-C', $upstream, 'rev-parse', 'HEAD')
    $marker = Get-Content -Raw -LiteralPath (Join-Path $upstream 'local-marker')
    $env:PATH = "$shim;$originalPath"

    foreach ($flags in @('--prebuilt', '--dry-run --prebuilt', '--prebuilt --source', '--source --prebuilt', '--minimal --prebuilt')) {
        $output = Invoke-Setup $outside $false ($flags -split ' ')
        Assert-Contains $output 'no verified prebuilt binary channel'
        Assert-Contains $output 'https://github.com/kckylechen1/zeroclaw.git'
        Assert-Contains $output 'setup.bat --source'
        if ($output.Contains('Checking prerequisites')) { throw 'Refusal reached prerequisites' }
    }

    foreach ($case in @(
        @{ Flags = @('--dry-run'); Mode = 'dist' },
        @{ Flags = @('--source', '--dry-run'); Mode = 'dist' },
        @{ Flags = @('--minimal', '--dry-run'); Mode = 'minimal' },
        @{ Flags = @('--dry-run', '--default'); Mode = 'default' },
        @{ Flags = @('--all', '--dry-run'); Mode = 'all' }
    )) {
        $output = Invoke-Setup $fork $true $case.Flags
        Assert-Contains $output "Building ZeroClaw ($($case.Mode) "
        Assert-Contains $output '[dry-run] Would run: cargo build --release --locked'
        Assert-Contains $output 'Dry run complete.'
    }

    foreach ($flags in @('--source', '--dry-run')) {
        $output = Invoke-Setup $upstream $false @($flags)
        Assert-Contains $output 'Source origin is not this fork'
        if ($output.Contains('Checking prerequisites')) { throw 'Origin mismatch reached prerequisites' }
    }
    if ((Invoke-Git @('-C', $upstream, 'rev-parse', 'HEAD')) -ne $upstreamHead) { throw 'Upstream HEAD changed' }
    if ((Get-Content -Raw -LiteralPath (Join-Path $upstream 'local-marker')) -ne $marker) { throw 'Upstream local edit changed' }
    if ((Invoke-Git @('-C', $upstream, 'remote', 'get-url', 'origin')) -ne 'https://github.com/zeroclaw-labs/zeroclaw.git') { throw 'Upstream origin changed' }

    $output = Invoke-Setup $outside $false @('--dry-run')
    Assert-Contains $output 'Run this script from the fork repository root'
    foreach ($origin in @('https://github.com/kckylechen1/zeroclaw', 'git@github.com:kckylechen1/zeroclaw.git', 'ssh://git@github.com/kckylechen1/zeroclaw.git')) {
        $null = Invoke-Git @('-C', $fork, 'remote', 'set-url', 'origin', $origin)
        $output = Invoke-Setup $fork $true @('--dry-run')
        Assert-Contains $output '[dry-run] Would run: cargo build --release --locked'
    }
    Write-Output "Windows source installer smoke: $checks scenarios passed."
} finally {
    $env:PATH = $originalPath
    $env:ZEROCLAW_SMOKE_LOG = $originalLog
    $env:ZEROCLAW_REAL_GIT = $originalRealGit
    if (Test-Path -LiteralPath $scratch) { Remove-Item -LiteralPath $scratch -Recurse -Force }
}
