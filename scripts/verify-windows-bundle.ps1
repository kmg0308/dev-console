param(
    [Parameter(Mandatory = $true)][string]$ProductName,
    [Parameter(Mandatory = $true)][string]$MainBinaryName,
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][ValidateSet("true", "false")][string]$RuntimeFeature,
    [string]$Target = "x86_64-pc-windows-msvc"
)

$ErrorActionPreference = "Stop"
$root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$release = Join-Path $root "target/$Target/release"
$setups = @(Get-ChildItem (Join-Path $release "bundle/nsis") -File -Filter "*.exe")
if ($setups.Count -ne 1) {
    throw "Expected exactly one NSIS setup, found $($setups.Count)"
}
$versionInfo = $setups[0].VersionInfo
if ($versionInfo.ProductName -cne $ProductName -or $versionInfo.ProductVersion -cne $Version) {
    throw "NSIS metadata mismatch: '$($versionInfo.ProductName)' '$($versionInfo.ProductVersion)'"
}

$sidecarDirectory = Join-Path $root "src-tauri/binaries"
$sidecars = if (Test-Path $sidecarDirectory -PathType Container) {
    @(Get-ChildItem $sidecarDirectory -File | Where-Object {
        $_.Name.EndsWith("-$Target.exe", [StringComparison]::Ordinal)
    })
} else {
    @()
}
$expectedSidecars = if ($RuntimeFeature -eq "true") {
    @("runtime-atlas-$Target.exe", "runtime-atlas-supervisor-$Target.exe")
} else {
    @()
}
$expectedSidecars = @($expectedSidecars | Sort-Object)
$actualSidecars = @($sidecars.Name | Sort-Object)
if ($sidecars.Count -ne $expectedSidecars.Count -or
    ($actualSidecars -join "`0") -cne ($expectedSidecars -join "`0") -or
    @($sidecars | Where-Object { $_.Attributes -band [IO.FileAttributes]::ReparsePoint }).Count -ne 0) {
    throw "Unexpected target sidecars: $($sidecars.Name -join ', ')"
}

$temporary = Join-Path ([IO.Path]::GetTempPath()) "dev-console-windows-qa-$([Guid]::NewGuid())"
$originalLocalAppData = $env:LOCALAPPDATA
$originalHome = $env:HOME
$originalCodexHome = $env:CODEX_HOME
$main = $null
try {
    $localAppData = Join-Path $temporary "local-app-data"
    $isolatedHome = Join-Path $temporary "home"
    New-Item -ItemType Directory -Force $localAppData, $isolatedHome | Out-Null
    $env:LOCALAPPDATA = $localAppData
    $env:HOME = $isolatedHome
    [Environment]::SetEnvironmentVariable("CODEX_HOME", $null, "Process")

    if ($RuntimeFeature -eq "true") {
        $cliPath = Join-Path $sidecarDirectory "runtime-atlas-$Target.exe"
        $cliOutput = Join-Path $temporary "runtime-atlas.stdout"
        $cliError = Join-Path $temporary "runtime-atlas.stderr"
        $cli = Start-Process $cliPath `
            -ArgumentList "help" -RedirectStandardOutput $cliOutput -RedirectStandardError $cliError `
            -NoNewWindow -Wait -PassThru
        if ($cli.ExitCode -ne 0 -or
            -not (Get-Content $cliOutput -Raw).StartsWith("Runtime Atlas reads local worktree and runtime state.")) {
            throw "runtime-atlas help contract failed"
        }

        $supervisorPath = Join-Path $sidecarDirectory "runtime-atlas-supervisor-$Target.exe"
        $supervisorOutput = Join-Path $temporary "supervisor.stdout"
        $supervisorError = Join-Path $temporary "supervisor.stderr"
        $supervisor = Start-Process $supervisorPath `
            -RedirectStandardOutput $supervisorOutput -RedirectStandardError $supervisorError `
            -NoNewWindow -Wait -PassThru
        if ($supervisor.ExitCode -ne 64 -or
            -not (Get-Content $supervisorError -Raw).StartsWith("usage: runtime-atlas-supervisor ")) {
            throw "runtime-atlas-supervisor usage contract failed"
        }
    }

    $mainPath = Join-Path $release "$MainBinaryName.exe"
    if (-not (Test-Path $mainPath -PathType Leaf)) {
        throw "Built main executable is missing: $mainPath"
    }
    $main = Start-Process $mainPath -PassThru
    Start-Sleep -Seconds 3
    $main.Refresh()
    if ($main.HasExited) {
        throw "Built main executable exited during the startup smoke test"
    }
    Stop-Process -Id $main.Id -Force
    $main.WaitForExit()
} finally {
    if ($null -ne $main -and -not $main.HasExited) {
        Stop-Process -Id $main.Id -Force -ErrorAction SilentlyContinue
        $main.WaitForExit()
    }
    $env:LOCALAPPDATA = $originalLocalAppData
    $env:HOME = $originalHome
    [Environment]::SetEnvironmentVariable("CODEX_HOME", $originalCodexHome, "Process")
    for ($attempt = 0; (Test-Path $temporary) -and $attempt -lt 5; $attempt++) {
        Remove-Item $temporary -Recurse -Force -ErrorAction SilentlyContinue
        if (Test-Path $temporary) {
            Start-Sleep -Seconds 1
        }
    }
    if (Test-Path $temporary) {
        throw "Could not clean up isolated startup data: $temporary"
    }
}
