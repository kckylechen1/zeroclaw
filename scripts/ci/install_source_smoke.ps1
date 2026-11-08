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

function Invoke-Capture {
    param([string] $FileName, [string] $Arguments, [string] $Directory)
    $start = [System.Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $FileName
    $start.Arguments = $Arguments
    $start.WorkingDirectory = $Directory
    $start.UseShellExecute = $false
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $start
    try {
        if (-not $process.Start()) { throw 'Could not start fixture process' }
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit(30000)) {
            $process.Kill($true)
            throw 'Fixture process hung or attempted an interactive install'
        }
        return [pscustomobject]@{
            ExitCode = $process.ExitCode
            Stdout = $stdout.GetAwaiter().GetResult()
            Stderr = $stderr.GetAwaiter().GetResult()
        }
    } finally {
        $process.Dispose()
    }
}

function Invoke-Setup {
    param([string] $Directory, [bool] $Success, [string[]] $Flags)
    $arguments = '/d /s /c ""' + $installer + '" ' + ($Flags -join ' ') + '"'
    $result = Invoke-Capture $env:ComSpec $arguments $Directory
    $output = $result.Stdout + $result.Stderr
    if (($result.ExitCode -eq 0) -ne $Success) {
        throw "Unexpected setup.bat exit $($result.ExitCode), fixture $(Split-Path $Directory -Leaf), flags $($Flags -join ' '): $output"
    }
    if ((Get-Item -LiteralPath $env:ZEROCLAW_SMOKE_LOG).Length -ne 0) {
        throw "Unexpected mutation: $(Get-Content -Raw -LiteralPath $env:ZEROCLAW_SMOKE_LOG)"
    }
    $script:checks++
    return $output
}

function Get-LineEnding {
    param([string] $Text)
    if ($Text.EndsWith("`r`n")) { return 'CRLF' }
    if ($Text.EndsWith("`n")) { return 'LF' }
    return 'none'
}

function Assert-FixtureOrigin {
    param([string] $Directory, [string] $Expected)
    # Only the two synthetic fixture origins are accepted by this diagnostic;
    # never print an inherited/re-written URL or global credential configuration.
    if ($Expected -notin @('https://github.com/kckylechen1/zeroclaw.git', 'https://github.com/zeroclaw-labs/zeroclaw.git')) {
        throw 'Origin diagnostic requires a known synthetic fixture'
    }
    $configured = Invoke-Git @('-C', $Directory, 'config', '--local', '--get', 'remote.origin.url')
    $native = Invoke-Capture $realGit 'remote get-url origin' $Directory
    $wrapped = Invoke-Capture $env:ComSpec '/d /s /c "git remote get-url origin"' $Directory
    $configuredMatches = $configured -ceq $Expected
    $nativeMatches = $native.Stdout.TrimEnd([char[]]"`r`n") -ceq $Expected
    $wrappedMatches = $wrapped.Stdout.TrimEnd([char[]]"`r`n") -ceq $Expected
    Write-Output "Fixture $(Split-Path $Directory -Leaf): configured_match=$configuredMatches; native_exit=$($native.ExitCode); native_match=$nativeMatches; native_eol=$(Get-LineEnding $native.Stdout); wrapper_exit=$($wrapped.ExitCode); wrapper_match=$wrappedMatches; wrapper_eol=$(Get-LineEnding $wrapped.Stdout)"
    if (-not $configuredMatches -or $native.ExitCode -ne 0 -or -not $nativeMatches -or $wrapped.ExitCode -ne 0 -or -not $wrappedMatches) {
        throw 'Synthetic origin read differs before installer validation; URL values are withheld'
    }
    # Reproduce the old matcher independently for native Windows evidence.
    # Its result is diagnostic; the installer still must pass every real test.
    $legacyCommand = '/d /s /c "git remote get-url origin 2>nul | findstr /I /X /L /C:"https://github.com/kckylechen1/zeroclaw" /C:"https://github.com/kckylechen1/zeroclaw.git" /C:"git@github.com:kckylechen1/zeroclaw" /C:"git@github.com:kckylechen1/zeroclaw.git" /C:"ssh://git@github.com/kckylechen1/zeroclaw" /C:"ssh://git@github.com/kckylechen1/zeroclaw.git" >nul"'
    $legacy = Invoke-Capture $env:ComSpec $legacyCommand $Directory
    Write-Output "Fixture $(Split-Path $Directory -Leaf): legacy_findstr_exit=$($legacy.ExitCode)"
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
    Assert-FixtureOrigin $fork 'https://github.com/kckylechen1/zeroclaw.git'
    Assert-FixtureOrigin $upstream 'https://github.com/zeroclaw-labs/zeroclaw.git'

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
    foreach ($origin in @('https://github.com/kckylechen1/zeroclaw', 'git@github.com:kckylechen1/zeroclaw', 'git@github.com:kckylechen1/zeroclaw.git', 'ssh://git@github.com/kckylechen1/zeroclaw', 'ssh://git@github.com/kckylechen1/zeroclaw.git', 'HTTPS://GITHUB.COM/KCKYLECHEN1/ZEROCLAW.GIT')) {
        $null = Invoke-Git @('-C', $fork, 'remote', 'set-url', 'origin', $origin)
        $output = Invoke-Setup $fork $true @('--dry-run')
        Assert-Contains $output '[dry-run] Would run: cargo build --release --locked'
    }
    foreach ($origin in @('https://github.com/kckylechen1/zeroclaw.git/extra', 'https://github.com/kckylechen1/zeroclaw.git ', "https://github.com/kckylechen1/zeroclaw.git`nhttps://example.invalid/extra")) {
        $null = Invoke-Git @('-C', $fork, 'remote', 'set-url', 'origin', $origin)
        $output = Invoke-Setup $fork $false @('--dry-run')
        Assert-Contains $output 'Source origin is not this fork'
        if ($output.Contains('Checking prerequisites')) { throw 'Invalid origin reached prerequisites' }
    }
    Write-Output "Windows source installer smoke: $checks scenarios passed."
} finally {
    $env:PATH = $originalPath
    $env:ZEROCLAW_SMOKE_LOG = $originalLog
    $env:ZEROCLAW_REAL_GIT = $originalRealGit
    if (Test-Path -LiteralPath $scratch) { Remove-Item -LiteralPath $scratch -Recurse -Force }
}
