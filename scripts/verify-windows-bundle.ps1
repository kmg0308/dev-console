param(
    [Parameter(Mandatory = $true)][string]$ProductName,
    [Parameter(Mandatory = $true)][string]$MainBinaryName,
    [Parameter(Mandatory = $true)][string]$BundleIdentifier,
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][ValidateSet("true", "false")][string]$RuntimeFeature,
    [ValidateSet("x86_64-pc-windows-msvc")][string]$Target = "x86_64-pc-windows-msvc",
    [string]$CertificateThumbprint,
    [string]$InstallerPath,
    [switch]$AllowWindowsServerSmoke
)

$ErrorActionPreference = "Stop"

function Assert-RegularFile([string]$Path, [string]$Description) {
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "$Description is missing: $Path"
    }
    $item = Get-Item -LiteralPath $Path -Force
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw "$Description must not be a reparse point: $Path"
    }
    $item
}

function Assert-RegularDirectory([string]$Path, [string]$Description) {
    if (-not (Test-Path -LiteralPath $Path -PathType Container)) {
        throw "$Description is missing: $Path"
    }
    $item = Get-Item -LiteralPath $Path -Force
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw "$Description must not be a reparse point: $Path"
    }
}

function ConvertFrom-QuotedRegistryPath([string]$Value, [string]$Description) {
    if ($Value.Length -lt 3 -or $Value[0] -ne '"' -or $Value[$Value.Length - 1] -ne '"') {
        throw "$Description must be one quoted path"
    }
    $path = $Value.Substring(1, $Value.Length - 2)
    if ($path.Contains('"')) {
        throw "$Description contains an unexpected quote"
    }
    [IO.Path]::GetFullPath($path)
}

function Assert-SamePath([string]$Actual, [string]$Expected, [string]$Description) {
    $actualPath = [IO.Path]::GetFullPath($Actual).TrimEnd([IO.Path]::DirectorySeparatorChar)
    $expectedPath = [IO.Path]::GetFullPath($Expected).TrimEnd([IO.Path]::DirectorySeparatorChar)
    if (-not [string]::Equals($actualPath, $expectedPath, [StringComparison]::OrdinalIgnoreCase)) {
        throw "$Description mismatch: '$actualPath' instead of '$expectedPath'"
    }
}

function Assert-AuthenticodeSignature([string]$Path, [string]$ExpectedThumbprint) {
    if (-not $ExpectedThumbprint) {
        return
    }
    $signature = Get-AuthenticodeSignature -LiteralPath $Path
    if ($signature.Status -ne "Valid" -or
        $null -eq $signature.SignerCertificate -or
        $signature.SignerCertificate.Thumbprint -cne $ExpectedThumbprint -or
        $null -eq $signature.TimeStamperCertificate) {
        throw "Authenticode signer, status, or timestamp mismatch: $Path"
    }
}

$os = Get-CimInstance Win32_OperatingSystem
$osVersion = [Version]$os.Version
$osArchitecture = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture
$processArchitecture = [Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture
if ($osVersion.Major -lt 10 -or ($osVersion.Major -eq 10 -and [int]$os.BuildNumber -lt 19045)) {
    throw "Windows build $($os.BuildNumber) is older than the Windows 10 22H2 support floor"
}
if ([int]$os.ProductType -ne 1 -and -not $AllowWindowsServerSmoke) {
    throw "Windows release verification requires a client OS; use -AllowWindowsServerSmoke only for CI package smoke tests"
}
if ($osArchitecture -ne [Runtime.InteropServices.Architecture]::X64 -or
    $processArchitecture -ne [Runtime.InteropServices.Architecture]::X64) {
    throw "Windows QA requires an x64 OS and x64 process, found $osArchitecture/$processArchitecture"
}
$hostRole = if ([int]$os.ProductType -eq 1) { "client verification" } else { "explicit server smoke" }
Write-Host "Windows QA host ($hostRole): $($os.Caption), version $($os.Version), build $($os.BuildNumber), $osArchitecture OS/$processArchitecture process"

$root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$release = Join-Path $root "target/$Target/release"
$setups = if ($InstallerPath) {
    @((Get-Item -LiteralPath $InstallerPath -Force))
} else {
    @(Get-ChildItem (Join-Path $release "bundle/nsis") -File -Filter "*.exe")
}
if ($setups.Count -ne 1) {
    throw "Expected exactly one NSIS setup, found $($setups.Count)"
}
$setupItem = Assert-RegularFile $setups[0].FullName "NSIS setup"
$versionInfo = $setupItem.VersionInfo
if ($versionInfo.ProductName -cne $ProductName -or $versionInfo.ProductVersion -cne $Version) {
    throw "NSIS metadata mismatch: '$($versionInfo.ProductName)' '$($versionInfo.ProductVersion)'"
}
$expectedThumbprint = $null
if ($CertificateThumbprint) {
    $expectedThumbprint = ($CertificateThumbprint -replace '\s', '').ToUpperInvariant()
    if ($expectedThumbprint -notmatch '^[0-9A-F]{40}$') {
        throw "CertificateThumbprint must contain exactly 40 hexadecimal characters"
    }
}
Assert-AuthenticodeSignature $setupItem.FullName $expectedThumbprint

$identifierParts = $BundleIdentifier.Split('.')
if ($identifierParts.Count -lt 2 -or [string]::IsNullOrWhiteSpace($identifierParts[1])) {
    throw "Bundle identifier cannot resolve the default Tauri manufacturer: $BundleIdentifier"
}
$manufacturer = $identifierParts[1]
$uninstallRegistryPath = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\$ProductName"
$productRegistryPath = "HKCU:\Software\$manufacturer\$ProductName"
$machineUninstallRoots = @(
    "HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall",
    "HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall"
)
foreach ($path in @(
    $uninstallRegistryPath,
    "$($machineUninstallRoots[0])\$ProductName",
    "$($machineUninstallRoots[1])\$ProductName"
)) {
    if (Test-Path -LiteralPath $path) {
        throw "Refusing to replace an existing $ProductName installation"
    }
}
foreach ($registryRoot in $machineUninstallRoots) {
    $legacy = @(Get-ChildItem -LiteralPath $registryRoot -ErrorAction SilentlyContinue | ForEach-Object {
        Get-ItemProperty -LiteralPath $_.PSPath -ErrorAction SilentlyContinue
    } | Where-Object { $_.DisplayName -ceq $ProductName -and $_.Publisher -ceq $manufacturer })
    if ($legacy.Count -ne 0) {
        throw "Refusing to replace an existing $ProductName machine installation"
    }
}
if (Test-Path -LiteralPath $productRegistryPath) {
    throw "Refusing to replace existing $ProductName installer state"
}
if (Get-Process -Name $MainBinaryName -ErrorAction SilentlyContinue) {
    throw "Refusing to install while an existing $MainBinaryName process is running"
}

$localDataRoot = [Environment]::GetFolderPath([Environment+SpecialFolder]::LocalApplicationData)
$appDataNames = switch ($BundleIdentifier) {
    "local.tokenmeter.app" { @("TokenMeter", $BundleIdentifier) }
    "com.kmg0308.runtimeatlas" { @("Runtime Atlas", $BundleIdentifier) }
    "com.kmg0308.devconsole" { @("TokenMeter", "Runtime Atlas", $BundleIdentifier) }
    default { throw "Unexpected bundle identifier: $BundleIdentifier" }
}
$appDataPaths = @($appDataNames | ForEach-Object { Join-Path $localDataRoot $_ })
foreach ($path in $appDataPaths) {
    if (Test-Path -LiteralPath $path) {
        throw "Refusing to replace existing application data: $path"
    }
}

$temporary = Join-Path ([IO.Path]::GetTempPath()) "dev-console-windows-qa-$([Guid]::NewGuid())"
$installDirectory = Join-Path $temporary "installed"
if ($installDirectory.Contains('"') -or $installDirectory.Contains("`r") -or $installDirectory.Contains("`n")) {
    throw "Temporary install path contains unsupported command-line characters"
}
$originalLocalAppData = $env:LOCALAPPDATA
$originalHome = $env:HOME
$originalCodexHome = $env:CODEX_HOME
$main = $null
$uninstaller = $null
try {
    $localAppData = Join-Path $temporary "local-app-data"
    $isolatedHome = Join-Path $temporary "home"
    New-Item -ItemType Directory -Force $localAppData, $isolatedHome | Out-Null

    $setup = Start-Process $setupItem.FullName `
        -ArgumentList "/S /NS /D=$installDirectory" -Wait -PassThru
    if ($setup.ExitCode -ne 0) {
        throw "NSIS installer failed with exit code $($setup.ExitCode)"
    }

    Assert-RegularDirectory $installDirectory "Installed application directory"
    if (-not (Test-Path -LiteralPath $uninstallRegistryPath)) {
        throw "NSIS installer did not create its official uninstall registration"
    }
    $registration = Get-ItemProperty -LiteralPath $uninstallRegistryPath
    if ($registration.DisplayName -cne $ProductName -or
        $registration.DisplayVersion -cne $Version -or
        $registration.MainBinaryName -cne "$MainBinaryName.exe" -or
        $registration.Publisher -cne $manufacturer) {
        throw "Installed application registry metadata mismatch"
    }
    $registeredInstall = ConvertFrom-QuotedRegistryPath $registration.InstallLocation "InstallLocation"
    $registeredUninstaller = ConvertFrom-QuotedRegistryPath $registration.UninstallString "UninstallString"
    $registeredMain = ConvertFrom-QuotedRegistryPath $registration.DisplayIcon "DisplayIcon"
    Assert-SamePath $registeredInstall $installDirectory "Registered install location"
    Assert-SamePath $registeredUninstaller (Join-Path $installDirectory "uninstall.exe") "Registered uninstaller"
    Assert-SamePath $registeredMain (Join-Path $installDirectory "$MainBinaryName.exe") "Registered main executable"

    $mainPath = Join-Path $registeredInstall "$MainBinaryName.exe"
    $mainItem = Assert-RegularFile $mainPath "Installed main executable"
    $uninstaller = (Assert-RegularFile $registeredUninstaller "Official uninstaller").FullName
    $installedVersion = $mainItem.VersionInfo
    if ($installedVersion.ProductName -cne $ProductName -or
        $installedVersion.ProductVersion -cne $Version) {
        throw "Installed main metadata mismatch: '$($installedVersion.ProductName)' '$($installedVersion.ProductVersion)'"
    }
    Assert-AuthenticodeSignature $mainItem.FullName $expectedThumbprint

    $expectedSidecars = if ($RuntimeFeature -eq "true") {
        @("runtime-atlas.exe", "runtime-atlas-supervisor.exe")
    } else {
        @()
    }
    $actualSidecars = @(Get-ChildItem -LiteralPath $registeredInstall -Force | Where-Object {
        $_.Extension -ieq ".exe" -and
        -not [string]::Equals($_.Name, "$MainBinaryName.exe", [StringComparison]::OrdinalIgnoreCase) -and
        -not [string]::Equals($_.Name, "uninstall.exe", [StringComparison]::OrdinalIgnoreCase)
    } | ForEach-Object Name | Sort-Object)
    $expectedSidecars = @($expectedSidecars | Sort-Object)
    if (($actualSidecars -join "`0") -cne ($expectedSidecars -join "`0")) {
        throw "Unexpected installed sidecars: $($actualSidecars -join ', ')"
    }
    foreach ($sidecar in $expectedSidecars) {
        $sidecarItem = Assert-RegularFile (Join-Path $registeredInstall $sidecar) "Installed $sidecar"
        Assert-AuthenticodeSignature $sidecarItem.FullName $expectedThumbprint
    }
    Write-Host "NSIS install verified: $ProductName at $registeredInstall"

    $env:LOCALAPPDATA = $localAppData
    $env:HOME = $isolatedHome
    [Environment]::SetEnvironmentVariable("CODEX_HOME", $null, "Process")

    if ($RuntimeFeature -eq "true") {
        $cliPath = Join-Path $registeredInstall "runtime-atlas.exe"
        $cliOutput = Join-Path $temporary "runtime-atlas.stdout"
        $cliError = Join-Path $temporary "runtime-atlas.stderr"
        $cli = Start-Process $cliPath `
            -ArgumentList "help" -RedirectStandardOutput $cliOutput -RedirectStandardError $cliError `
            -NoNewWindow -Wait -PassThru
        if ($cli.ExitCode -ne 0 -or
            -not (Get-Content $cliOutput -Raw).StartsWith("Runtime Atlas reads local worktree and runtime state.")) {
            throw "runtime-atlas help contract failed"
        }

        $supervisorPath = Join-Path $registeredInstall "runtime-atlas-supervisor.exe"
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

    $main = Start-Process $mainPath -PassThru
    Start-Sleep -Seconds 3
    $main.Refresh()
    if ($main.HasExited) {
        throw "Installed main executable exited during the startup smoke test"
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
    try {
        if ($null -eq $uninstaller) {
            $candidate = Join-Path $installDirectory "uninstall.exe"
            if (Test-Path -LiteralPath $candidate -PathType Leaf) {
                $uninstaller = (Assert-RegularFile $candidate "Official uninstaller").FullName
            }
        }
        if ($null -ne $uninstaller) {
            $uninstall = Start-Process $uninstaller -ArgumentList "/S" -Wait -PassThru
            if ($uninstall.ExitCode -ne 0) {
                throw "Official uninstaller failed with exit code $($uninstall.ExitCode)"
            }
            for ($attempt = 0; (Test-Path -LiteralPath $installDirectory) -and $attempt -lt 10; $attempt++) {
                Start-Sleep -Milliseconds 500
            }
            if (Test-Path -LiteralPath $installDirectory) {
                throw "Official uninstaller did not remove the installed application"
            }
            if (Test-Path -LiteralPath $uninstallRegistryPath) {
                throw "Official uninstaller did not remove its uninstall registration"
            }
            Write-Host "NSIS uninstall verified: $ProductName"
        }
    } finally {
        try {
            foreach ($path in $appDataPaths) {
                if (-not (Test-Path -LiteralPath $path)) {
                    continue
                }
                Assert-RegularDirectory $path "QA application data directory"
                $reparsePoints = @(Get-ChildItem -LiteralPath $path -Force -Recurse | Where-Object {
                    $_.Attributes -band [IO.FileAttributes]::ReparsePoint
                })
                if ($reparsePoints.Count -ne 0) {
                    throw "Refusing to remove QA application data containing a reparse point: $path"
                }
                for ($attempt = 0; (Test-Path -LiteralPath $path) -and $attempt -lt 10; $attempt++) {
                    Remove-Item -LiteralPath $path -Recurse -Force -ErrorAction SilentlyContinue
                    if (Test-Path -LiteralPath $path) {
                        Start-Sleep -Milliseconds 500
                    }
                }
                if (Test-Path -LiteralPath $path) {
                    throw "Could not clean up QA application data: $path"
                }
            }
            if (Test-Path -LiteralPath $productRegistryPath) {
                $productKey = Get-Item -LiteralPath $productRegistryPath
                $rememberedInstall = [string]$productKey.GetValue("")
                $valueNames = @($productKey.GetValueNames() | Where-Object { $_ -notin @("", "Installer Language") })
                if ($productKey.SubKeyCount -ne 0 -or $valueNames.Count -ne 0) {
                    throw "Refusing to remove unexpected $ProductName installer state"
                }
                Assert-SamePath $rememberedInstall $installDirectory "Remembered install location"
                $productKey.Close()
                Remove-Item -LiteralPath $productRegistryPath -Force
            }
        } finally {
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
    }
}
