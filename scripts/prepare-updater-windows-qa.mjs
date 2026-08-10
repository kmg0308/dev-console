import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import { createReadStream } from "node:fs";
import {
  chmod,
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  realpath,
  rename,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { basename, dirname, isAbsolute, join, relative, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const prefix = "dev-console-windows-updater-qa-";
const targetTriple = "x86_64-pc-windows-msvc";
const platform = "windows-x86_64";
const command = process.argv[2] || "start";
const flavors = {
  "token-meter": {
    productName: "TokenMeter",
    binaryName: "TokenMeter",
    identifier: "local.tokenmeter.app",
    appDataNames: ["TokenMeter", "local.tokenmeter.app"],
    tokenMeter: true,
    runtime: false,
  },
  "runtime-atlas": {
    productName: "RuntimeAtlas",
    binaryName: "RuntimeAtlas",
    identifier: "com.kmg0308.runtimeatlas",
    appDataNames: ["Runtime Atlas", "com.kmg0308.runtimeatlas"],
    tokenMeter: false,
    runtime: true,
  },
  "dev-console": {
    productName: "DevConsole",
    binaryName: "DevConsole",
    identifier: "com.kmg0308.devconsole",
    appDataNames: ["TokenMeter", "Runtime Atlas", "com.kmg0308.devconsole"],
    tokenMeter: true,
    runtime: true,
  },
};

const productionProbeScript = String.raw`
$ErrorActionPreference = "Stop"
$product = $env:QA_PRODUCT
$identifier = $env:QA_IDENTIFIER
$processNames = @(ConvertFrom-Json $env:QA_PROCESS_NAMES)
$appDataNames = @(ConvertFrom-Json $env:QA_APP_DATA_NAMES)
$installerStatePath = $env:QA_INSTALLER_STATE_PATH
$localAppData = [Environment]::GetFolderPath([Environment+SpecialFolder]::LocalApplicationData)
$appDataPaths = @($appDataNames | ForEach-Object { Join-Path -Path $localAppData -ChildPath $_ })
$roots = @(
  @{ hive = "HKCU"; path = "Registry::HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Uninstall" },
  @{ hive = "HKLM"; path = "Registry::HKEY_LOCAL_MACHINE\Software\Microsoft\Windows\CurrentVersion\Uninstall" },
  @{ hive = "HKLM"; path = "Registry::HKEY_LOCAL_MACHINE\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall" }
)
$installs = @()
foreach ($root in $roots) {
  if (-not (Test-Path -LiteralPath $root.path)) { continue }
  foreach ($key in Get-ChildItem -LiteralPath $root.path) {
    $item = Get-ItemProperty -LiteralPath $key.PSPath
    if ($item.DisplayName -eq $product -or $key.PSChildName -eq $identifier) {
      $installs += [PSCustomObject]@{
        hive = $root.hive
        key = $key.PSChildName
        displayName = [string]$item.DisplayName
        installLocation = [string]$item.InstallLocation
        uninstallString = [string]$item.UninstallString
      }
    }
  }
}
$processes = @(Get-CimInstance Win32_Process | Where-Object { $processNames -contains $_.Name } | ForEach-Object {
  [PSCustomObject]@{ id = $_.ProcessId; name = $_.Name; path = [string]$_.ExecutablePath }
})
$appData = @($appDataPaths | Where-Object { Test-Path -LiteralPath $_ })
$installerState = $null
if (Test-Path -LiteralPath $installerStatePath) {
  $key = Get-Item -LiteralPath $installerStatePath
  $installerState = [PSCustomObject]@{
    rememberedInstall = [string]$key.GetValue("")
    valueNames = @($key.GetValueNames() | Where-Object { $_ -notin @("", "Installer Language") })
    subKeyCount = $key.SubKeyCount
  }
  $key.Close()
}
$os = Get-CimInstance Win32_OperatingSystem
$hostInfo = [PSCustomObject]@{
  build = [int]$os.BuildNumber
  productType = [int]$os.ProductType
  osArchitecture = [string][Runtime.InteropServices.RuntimeInformation]::OSArchitecture
  processArchitecture = [string][Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture
  localAppData = $localAppData
}
[PSCustomObject]@{
  installs = @($installs)
  processes = @($processes)
  appData = @($appData)
  installerState = $installerState
  host = $hostInfo
} |
  ConvertTo-Json -Compress -Depth 5
`;

const removeInstallerStateScript = String.raw`
$ErrorActionPreference = "Stop"
$path = $env:QA_INSTALLER_STATE_PATH
if (-not (Test-Path -LiteralPath $path)) { exit 0 }
$key = Get-Item -LiteralPath $path
$remembered = [string]$key.GetValue("")
$extraValues = @($key.GetValueNames() | Where-Object { $_ -notin @("", "Installer Language") })
if ($key.SubKeyCount -ne 0 -or $extraValues.Count -ne 0 -or
    -not [string]::Equals(
      [IO.Path]::GetFullPath($remembered).TrimEnd([IO.Path]::DirectorySeparatorChar),
      [IO.Path]::GetFullPath($env:QA_INSTALL_DIRECTORY).TrimEnd([IO.Path]::DirectorySeparatorChar),
      [StringComparison]::OrdinalIgnoreCase
    )) {
  $key.Close()
  throw "Refusing to remove unexpected installer state"
}
$key.Close()
Remove-Item -LiteralPath $path -Force
`;

const exactProcessScript = String.raw`
$ErrorActionPreference = "Stop"
$targets = @(ConvertFrom-Json $env:QA_PROCESS_PATHS | ForEach-Object { [IO.Path]::GetFullPath($_).ToLowerInvariant() })
$matches = @(Get-CimInstance Win32_Process | Where-Object {
  $_.ExecutablePath -and ($targets -contains [IO.Path]::GetFullPath($_.ExecutablePath).ToLowerInvariant())
})
if ($env:QA_STOP_PROCESSES -eq "1") {
  foreach ($process in $matches) { Stop-Process -Id $process.ProcessId -Force -ErrorAction Stop }
  Start-Sleep -Milliseconds 200
  $matches = @(Get-CimInstance Win32_Process | Where-Object {
    $_.ExecutablePath -and ($targets -contains [IO.Path]::GetFullPath($_.ExecutablePath).ToLowerInvariant())
  })
  if ($matches.Count -ne 0) { throw "Exact installed-path processes are still running" }
}
@($matches | ForEach-Object { [PSCustomObject]@{ id = $_.ProcessId; path = $_.ExecutablePath } }) |
  ConvertTo-Json -Compress -Depth 3
`;

const versionInfoScript = String.raw`
$ErrorActionPreference = "Stop"
$item = Get-Item -LiteralPath $env:QA_EXECUTABLE
[PSCustomObject]@{
  productName = $item.VersionInfo.ProductName
  productVersion = $item.VersionInfo.ProductVersion
} | ConvertTo-Json -Compress
`;

function flavor(name) {
  const value = flavors[name];
  if (!value) throw new Error(`Invalid flavor: ${name || "(missing)"}`);
  return value;
}

function array(value) {
  return value === undefined || value === null ? [] : Array.isArray(value) ? value : [value];
}

function assertProductionAbsent(probe) {
  const installs = array(probe.installs);
  const processes = array(probe.processes);
  const appData = array(probe.appData);
  if (installs.length || processes.length || appData.length || probe.installerState) {
    throw new Error(
      `Production state is present; refusing QA (installs=${installs.length}, processes=${processes.length}, appData=${appData.length}, installerState=${Boolean(probe.installerState)})`,
    );
  }
}

function assertWindowsHost(host) {
  if (
    !Number.isInteger(host?.build) ||
    host.build < 19045 ||
    host.productType !== 1 ||
    host.osArchitecture !== "X64" ||
    host.processArchitecture !== "X64" ||
    typeof host.localAppData !== "string" ||
    !isAbsolute(host.localAppData)
  ) {
    throw new Error("Windows updater QA requires Windows 10 22H2+ on an x64 OS and x64 process");
  }
}

function quotedRegistryPath(value, description) {
  const path = typeof value === "string" ? value : "";
  if (
    path !== path.trim() ||
    path.length < 3 ||
    path[0] !== '"' ||
    path.at(-1) !== '"' ||
    path.slice(1, -1).includes('"')
  ) {
    throw new Error(`${description} must be one quoted path`);
  }
  const unquoted = path.slice(1, -1);
  if (!isAbsolute(unquoted)) throw new Error(`${description} must be one quoted absolute path`);
  return unquoted;
}

function assertQaInstallRecord(probe, directory) {
  const installs = array(probe.installs);
  if (installs.length !== 1 || installs[0].hive !== "HKCU") {
    throw new Error("Expected exactly one current-user QA install record");
  }
  const location = quotedRegistryPath(installs[0].installLocation, "InstallLocation");
  const uninstaller = quotedRegistryPath(installs[0].uninstallString, "UninstallString");
  if (!samePath(location, directory)) {
    throw new Error("QA install record does not point to the isolated install directory");
  }
  if (!samePath(uninstaller, join(directory, "uninstall.exe"))) {
    throw new Error("QA install record does not point to the exact official uninstaller");
  }
}

function samePath(left, right) {
  const normalized = (value) => {
    const path = resolve(value);
    return process.platform === "win32" ? path.toLowerCase() : path;
  };
  return normalized(left) === normalized(right);
}

function assertQaState(probe, directory, processPaths) {
  assertQaInstallRecord(probe, directory);
  if (array(probe.appData).length) {
    throw new Error("Production app data appeared during QA");
  }
  for (const process of array(probe.processes)) {
    if (!process.path || !processPaths.some((path) => samePath(path, process.path))) {
      throw new Error("A production-name process is running outside the isolated QA install");
    }
  }
  if (
    probe.installerState &&
    (probe.installerState.subKeyCount !== 0 ||
      array(probe.installerState.valueNames).length ||
      !samePath(probe.installerState.rememberedInstall, directory))
  ) {
    throw new Error("Unexpected production-name installer state appeared during QA");
  }
}

function qaOverlay(publicKey, endpoint, version) {
  return {
    version,
    bundle: {
      createUpdaterArtifacts: true,
      windows: { nsis: { installMode: "currentUser" } },
    },
    plugins: {
      updater: {
        pubkey: publicKey,
        endpoints: [endpoint],
        dangerousInsecureTransportProtocol: true,
      },
    },
  };
}

function qaBuildEnvironment(environment, dataRoot, flavorName) {
  return {
    ...environment,
    DEV_CONSOLE_WINDOWS_UPDATER_QA_ROOT: dataRoot,
    DEV_CONSOLE_WINDOWS_UPDATER_QA_FLAVOR: flavorName,
  };
}

function updateManifest(signature, url) {
  return {
    version: "0.1.1",
    notes: "Ephemeral local Windows updater QA",
    pub_date: new Date().toISOString(),
    platforms: { [platform]: { signature, url } },
  };
}

function tamper(signature) {
  const encoded = signature.trim();
  const lines = Buffer.from(encoded, "base64").toString("utf8").split("\n");
  if (lines.length < 4 || !/^[A-Za-z0-9+/]+={0,2}$/.test(lines[1])) {
    throw new Error("Unexpected updater signature format");
  }
  const index = Math.floor(lines[1].length / 2);
  lines[1] = `${lines[1].slice(0, index)}${lines[1][index] === "A" ? "B" : "A"}${lines[1].slice(index + 1)}`;
  const changed = Buffer.from(lines.join("\n")).toString("base64");
  assert.notEqual(changed, encoded);
  return changed;
}

function encodedPowerShell(script) {
  return Buffer.from(script, "utf16le").toString("base64");
}

function run(program, args, options = {}) {
  return new Promise((accept, reject) => {
    const child = spawn(program, args, {
      cwd: root,
      env: options.env,
      stdio: options.quiet ? ["ignore", "ignore", "inherit"] : "inherit",
      windowsHide: true,
    });
    options.onChild?.(child);
    child.once("error", reject);
    child.once("exit", (status, signal) => {
      options.onChild?.(null);
      status === 0 ? accept() : reject(new Error(`${program} failed (${signal || status})`));
    });
  });
}

function capture(program, args, env = process.env) {
  return new Promise((accept, reject) => {
    const child = spawn(program, args, {
      cwd: root,
      env,
      stdio: ["ignore", "pipe", "pipe"],
      windowsHide: true,
    });
    let stdout = "";
    let stderr = "";
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk) => (stdout += chunk));
    child.stderr.on("data", (chunk) => (stderr += chunk));
    child.once("error", reject);
    child.once("exit", (status) =>
      status === 0
        ? accept(stdout.trim())
        : reject(new Error(`${program} failed (${status}): ${stderr.trim()}`)),
    );
  });
}

async function powershell(script, env = process.env) {
  return capture(
    "powershell.exe",
    ["-NoLogo", "-NoProfile", "-NonInteractive", "-EncodedCommand", encodedPowerShell(script)],
    env,
  );
}

async function productionProbe(config) {
  const processNames = [
    `${config.binaryName}.exe`,
    ...(config.runtime ? ["runtime-atlas.exe", "runtime-atlas-supervisor.exe"] : []),
  ];
  const output = await powershell(productionProbeScript, {
    ...process.env,
    QA_PRODUCT: config.productName,
    QA_IDENTIFIER: config.identifier,
    QA_PROCESS_NAMES: JSON.stringify(processNames),
    QA_APP_DATA_NAMES: JSON.stringify(config.appDataNames),
    QA_INSTALLER_STATE_PATH: `Registry::HKEY_CURRENT_USER\\Software\\${config.identifier.split(".")[1]}\\${config.productName}`,
  });
  return JSON.parse(output);
}

async function removeInstallerState(config, installDirectory) {
  await powershell(removeInstallerStateScript, {
    ...process.env,
    QA_INSTALLER_STATE_PATH: `Registry::HKEY_CURRENT_USER\\Software\\${config.identifier.split(".")[1]}\\${config.productName}`,
    QA_INSTALL_DIRECTORY: installDirectory,
  });
}

async function exactProcesses(paths, stop = false) {
  const output = await powershell(exactProcessScript, {
    ...process.env,
    QA_PROCESS_PATHS: JSON.stringify(paths),
    QA_STOP_PROCESSES: stop ? "1" : "0",
  });
  return output ? array(JSON.parse(output)) : [];
}

async function productInfo(path) {
  return JSON.parse(
    await powershell(versionInfoScript, { ...process.env, QA_EXECUTABLE: path }),
  );
}

async function qaDirectory(value) {
  const directory = await realpath(resolve(value));
  const temporaryRoot = await realpath(tmpdir());
  if (!samePath(dirname(directory), temporaryRoot) || !basename(directory).startsWith(prefix)) {
    throw new Error("Refusing a QA directory outside the system temporary directory");
  }
  return directory;
}

function inside(directory, path) {
  const offset = relative(resolve(directory), resolve(path));
  return offset !== "" && !offset.startsWith("..") && !isAbsolute(offset);
}

async function regular(path) {
  const metadata = await lstat(path);
  if (!metadata.isFile() || metadata.isSymbolicLink()) {
    throw new Error(`Expected a regular non-symlink file: ${path}`);
  }
}

async function atomicWrite(path, contents, mode = 0o600) {
  const temporary = `${path}.${randomBytes(8).toString("hex")}`;
  await writeFile(temporary, contents, { flag: "wx", mode });
  await rename(temporary, path);
}

async function sha256(path) {
  const hash = createHash("sha256");
  for await (const chunk of createReadStream(path)) hash.update(chunk);
  return hash.digest("hex");
}

async function oneFile(directory, predicate, description) {
  const matches = (await readdir(directory)).filter(predicate);
  if (matches.length !== 1) {
    throw new Error(`Expected exactly one ${description}, found ${matches.length}`);
  }
  const path = join(directory, matches[0]);
  await regular(path);
  return path;
}

async function readControl(directory) {
  directory = await qaDirectory(directory);
  const control = JSON.parse(await readFile(join(directory, "control.json"), "utf8"));
  if (!samePath(control.directory, directory) || !inside(directory, control.installDirectory)) {
    throw new Error("Invalid Windows updater QA control paths");
  }
  const expected = flavor(control.flavor);
  if (control.identifier !== expected.identifier) throw new Error("Invalid Windows updater QA identity");
  const urls = [control.installUrl, control.launchUrl, control.verifyUrl, control.stopUrl].map(
    (value) => new URL(value),
  );
  if (
    urls.some(
      (url) =>
        url.protocol !== "http:" ||
        url.hostname !== "127.0.0.1" ||
        !url.port ||
        url.username ||
        url.password ||
        url.hash ||
        url.search,
    ) ||
    urls.some((url) => url.origin !== urls[0].origin)
  ) {
    throw new Error("Windows updater QA controls must use one exact loopback origin");
  }
  return { directory, control };
}

async function request(directory, key, options = {}) {
  const loaded = await readControl(directory);
  const response = await fetch(loaded.control[key], options);
  if (!response.ok) throw new Error(`${key} failed with HTTP ${response.status}: ${await response.text()}`);
  return response.status === 204 ? undefined : response.json();
}

async function setMode(directory, mode) {
  if (mode !== "valid" && mode !== "tampered") throw new Error("Mode must be valid or tampered");
  directory = await qaDirectory(directory);
  await stat(join(directory, "mode"));
  await atomicWrite(join(directory, "mode"), `${mode}\n`);
  console.log(`Windows updater QA mode: ${mode}`);
}

async function install(directory) {
  const result = await request(directory, "installUrl", { method: "POST" });
  console.log(`Installed ${result.productName} ${result.productVersion} in the isolated QA path.`);
}

async function launch(directory) {
  const result = await request(directory, "launchUrl", { method: "POST" });
  console.log(`Launched installed ${result.productName} at exact PID ${result.pid}.`);
}

async function verify(directory, version) {
  if (version !== "0.1.0" && version !== "0.1.1") {
    throw new Error("Expected version must be 0.1.0 or 0.1.1");
  }
  const { control } = await readControl(directory);
  const url = new URL(control.verifyUrl);
  url.searchParams.set("version", version);
  const response = await fetch(url);
  if (!response.ok) throw new Error(`verifyUrl failed with HTTP ${response.status}: ${await response.text()}`);
  const result = await response.json();
  console.log(`Verified installed ${result.productName} ${result.productVersion}, PID ${result.pid}, and sidecars.`);
}

async function stop(directory) {
  await request(directory, "stopUrl", { method: "POST" });
  console.log("Exact installed-path processes stopped and the official uninstaller completed; QA files are being removed.");
}

async function selfTest() {
  assert.match(productionProbeScript, /\[Environment\]::GetFolderPath/);
  assert(!productionProbeScript.includes("$env:LOCALAPPDATA"));
  for (const [name, expected] of Object.entries(flavors)) {
    const config = JSON.parse(await readFile(join(root, "apps", name, "tauri.conf.json"), "utf8"));
    assert.equal(config.productName, expected.productName);
    assert.equal(config.mainBinaryName, expected.binaryName);
    assert.equal(config.identifier, expected.identifier);
  }
  assert.throws(() => flavor("other"), /Invalid flavor/);
  assert.throws(
    () => assertProductionAbsent({ installs: [{ hive: "HKCU" }], processes: [], appData: [], installerState: null }),
    /refusing QA/,
  );
  assert.throws(
    () => assertProductionAbsent({ installs: [], processes: [{ id: 1 }], appData: [], installerState: null }),
    /refusing QA/,
  );
  assert.throws(
    () => assertProductionAbsent({ installs: [], processes: [], appData: ["C:\\data"], installerState: null }),
    /refusing QA/,
  );
  assert.throws(
    () => assertProductionAbsent({ installs: [], processes: [], appData: [], installerState: {} }),
    /refusing QA/,
  );
  assertProductionAbsent({ installs: [], processes: [], appData: [], installerState: null });
  assertWindowsHost({
    build: 19045,
    productType: 1,
    osArchitecture: "X64",
    processArchitecture: "X64",
    localAppData: tmpdir(),
  });
  assert.throws(
    () => assertWindowsHost({
      build: 19044,
      productType: 1,
      osArchitecture: "X64",
      processArchitecture: "X64",
      localAppData: tmpdir(),
    }),
    /Windows 10 22H2/,
  );
  assert.throws(
    () => assertWindowsHost({
      build: 20348,
      productType: 3,
      osArchitecture: "X64",
      processArchitecture: "X64",
      localAppData: tmpdir(),
    }),
    /Windows 10 22H2/,
  );
  const qaInstall = join(tmpdir(), "qa-install");
  const installProbe = {
    installs: [{
      hive: "HKCU",
      installLocation: `"${qaInstall}"`,
      uninstallString: `"${join(qaInstall, "uninstall.exe")}"`,
    }],
  };
  assertQaInstallRecord(installProbe, qaInstall);
  assert.throws(
    () => assertQaInstallRecord({ installs: [{ ...installProbe.installs[0], installLocation: qaInstall }] }, qaInstall),
    /quoted path/,
  );
  assert.throws(
    () => assertQaInstallRecord({ installs: [{ ...installProbe.installs[0], installLocation: `"${qaInstall}` }] }, qaInstall),
    /quoted path/,
  );
  const overlay = qaOverlay("public", "http://127.0.0.1:1234/latest.json", "0.1.1");
  assert.equal(overlay.bundle.windows.nsis.installMode, "currentUser");
  assert.equal(overlay.bundle.createUpdaterArtifacts, true);
  assert.equal(overlay.plugins.updater.dangerousInsecureTransportProtocol, true);
  assert(!JSON.stringify(overlay).match(/thumbprint|timestamp|authenticode/i));
  assert.deepEqual(
    qaBuildEnvironment({ KEEP: "yes" }, qaInstall, "dev-console"),
    {
      KEEP: "yes",
      DEV_CONSOLE_WINDOWS_UPDATER_QA_ROOT: qaInstall,
      DEV_CONSOLE_WINDOWS_UPDATER_QA_FLAVOR: "dev-console",
    },
  );
  const signature = Buffer.from("untrusted comment\nAAAA\ntrusted comment\nBBBB").toString("base64");
  const manifest = updateManifest(signature, "http://127.0.0.1:1234/update.exe");
  assert.deepEqual(Object.keys(manifest.platforms), [platform]);
  assert.notEqual(tamper(signature), signature);
  assert(samePath(join(tmpdir(), "qa"), join(tmpdir(), "qa")));
  assert(inside(join(tmpdir(), "qa"), join(tmpdir(), "qa", "installed")));
  assert(!inside(join(tmpdir(), "qa"), join(tmpdir(), "production")));
  process.stdout.write("Windows updater QA self-test passed\n");
}

async function start(flavorName) {
  if (process.platform !== "win32" || process.arch !== "x64") {
    throw new Error("This QA harness requires Windows x64");
  }
  if (process.argv.length !== 4) throw new Error("start requires one flavor");
  const config = flavor(flavorName);
  const initialProbe = await productionProbe(config);
  assertWindowsHost(initialProbe.host);
  assertProductionAbsent(initialProbe);

  const directory = await mkdtemp(join(await realpath(tmpdir()), prefix));
  await chmod(directory, 0o700);
  const isolatedHome = join(directory, "home");
  const localAppData = join(directory, "local-app-data");
  const codexHome = join(directory, "codex-home");
  const dataRoot = join(directory, "data");
  const keys = join(directory, "keys");
  const installDirectory = join(directory, "installed");
  for (const path of [isolatedHome, localAppData, codexHome, dataRoot, keys]) {
    await mkdir(path, { recursive: true, mode: 0o700 });
  }

  let activeChild;
  let server;
  let cleaning = false;
  let modePath;
  let validManifestPath;
  let tamperedManifestPath;
  let updateArtifact;
  const installedBinary = join(installDirectory, `${config.binaryName}.exe`);
  const installedProcessPaths = [
    installedBinary,
    ...(config.runtime
      ? [join(installDirectory, "runtime-atlas.exe"), join(installDirectory, "runtime-atlas-supervisor.exe")]
      : []),
  ];

  const validateInstallDirectory = async () => {
    const metadata = await lstat(installDirectory);
    if (!metadata.isDirectory() || metadata.isSymbolicLink()) {
      throw new Error("QA install path is not a normal directory");
    }
    const canonical = await realpath(installDirectory);
    if (!samePath(canonical, installDirectory) || !inside(directory, canonical)) {
      throw new Error("QA install path escaped the temporary workspace");
    }
  };

  const verifyInstalled = async (version, requireRunning) => {
    await validateInstallDirectory();
    await regular(installedBinary);
    const info = await productInfo(installedBinary);
    if (info.productName !== config.productName || info.productVersion !== version) {
      throw new Error(`Installed metadata mismatch: ${info.productName} ${info.productVersion}`);
    }
    for (const helper of ["runtime-atlas.exe", "runtime-atlas-supervisor.exe"]) {
      const path = join(installDirectory, helper);
      if (config.runtime) await regular(path);
      else await assert.rejects(lstat(path), { code: "ENOENT" });
    }
    const processes = await exactProcesses(installedProcessPaths);
    const mainProcess = processes.find((process) => samePath(process.path, installedBinary));
    if (requireRunning && !mainProcess) {
      throw new Error("Installed application did not restart from the exact QA path");
    }
    assertQaState(
      await productionProbe(config),
      installDirectory,
      installedProcessPaths,
    );
    if (requireRunning) {
      if (config.tokenMeter) await stat(join(dataRoot, "TokenMeter"));
      if (config.runtime) await stat(join(dataRoot, "Runtime Atlas"));
      await stat(join(dataRoot, "webview", "main"));
    }
    return { productName: info.productName, productVersion: info.productVersion, pid: mainProcess?.id };
  };

  const stopProcesses = async () => {
    await exactProcesses(installedProcessPaths, true);
  };

  const uninstall = async () => {
    try {
      await validateInstallDirectory();
    } catch (error) {
      if (error.code === "ENOENT") {
        assertProductionAbsent(await productionProbe(config));
        return;
      }
      throw error;
    }
    assertQaState(
      await productionProbe(config),
      installDirectory,
      installedProcessPaths,
    );
    await stopProcesses();
    const uninstaller = await oneFile(
      installDirectory,
      (name) => /^uninstall.*\.exe$/i.test(name),
      "official uninstaller",
    );
    if (!inside(installDirectory, uninstaller)) throw new Error("Uninstaller escaped the QA install directory");
    await run(uninstaller, ["/S"], { onChild: (child) => (activeChild = child) });
    for (let attempt = 0; attempt < 100; attempt += 1) {
      const probe = await productionProbe(config);
      if (array(probe.installs).length === 0) break;
      await delay(100);
    }
    if (array((await productionProbe(config)).installs).length) {
      throw new Error("Official uninstaller left the production-name registry record behind");
    }
    await removeInstallerState(config, installDirectory);
    assertProductionAbsent(await productionProbe(config));
  };

  const finishCleanup = async () => {
    if (server?.listening) await new Promise((done) => server.close(done));
    await rm(directory, { recursive: true, force: true });
  };

  const cleanup = async () => {
    if (cleaning) return;
    cleaning = true;
    activeChild?.kill();
    await uninstall();
    await finishCleanup();
  };

  const interrupted = () =>
    void cleanup()
      .then(() => process.exit(130))
      .catch((error) => {
        console.error(`QA cleanup failed; preserved ${directory}: ${error.message}`);
        process.exit(1);
      });
  process.once("SIGINT", interrupted);
  process.once("SIGTERM", interrupted);

  try {
    const stopToken = randomBytes(32).toString("hex");
    modePath = join(directory, "mode");
    validManifestPath = join(directory, "latest-valid.json");
    tamperedManifestPath = join(directory, "latest-tampered.json");
    let baseSetup;

    server = createServer(async (request, response) => {
      try {
        const url = new URL(request.url, "http://127.0.0.1");
        if (request.method === "POST" && url.pathname === `/install/${stopToken}`) {
          assertProductionAbsent(await productionProbe(config));
          await assert.rejects(stat(installDirectory), { code: "ENOENT" });
          await run(baseSetup, ["/S", "/NS", `/D=${installDirectory}`], {
            onChild: (child) => (activeChild = child),
          });
          const verified = await verifyInstalled("0.1.0", false);
          response.writeHead(200, { "content-type": "application/json", "cache-control": "no-store" });
          return response.end(JSON.stringify(verified));
        }
        if (request.method === "POST" && url.pathname === `/launch/${stopToken}`) {
          if ((await exactProcesses(installedProcessPaths)).length) return response.writeHead(409).end();
          const child = spawn(installedBinary, [], {
            detached: true,
            env: { ...process.env, HOME: isolatedHome, LOCALAPPDATA: localAppData, CODEX_HOME: codexHome },
            stdio: "ignore",
            windowsHide: false,
          });
          await new Promise((accept, reject) => {
            child.once("spawn", accept);
            child.once("error", reject);
          });
          child.unref();
          for (let attempt = 0; attempt < 50; attempt += 1) {
            const running = await exactProcesses(installedProcessPaths);
            if (running.length) {
              await verifyInstalled((await productInfo(installedBinary)).productVersion, true);
              response.writeHead(200, { "content-type": "application/json", "cache-control": "no-store" });
              return response.end(JSON.stringify({ productName: config.productName, pid: running[0].id }));
            }
            await delay(100);
          }
          throw new Error("Installed application did not start from the exact QA path");
        }
        if (request.method === "GET" && url.pathname === `/verify/${stopToken}`) {
          const version = url.searchParams.get("version");
          if (version !== "0.1.0" && version !== "0.1.1") return response.writeHead(400).end();
          const verified = await verifyInstalled(version, true);
          response.writeHead(200, { "content-type": "application/json", "cache-control": "no-store" });
          return response.end(JSON.stringify(verified));
        }
        if (request.method === "POST" && url.pathname === `/stop/${stopToken}`) {
          if (cleaning) return response.writeHead(409).end();
          cleaning = true;
          try {
            await uninstall();
          } catch (error) {
            cleaning = false;
            throw error;
          }
          response.writeHead(204).end();
          setImmediate(() =>
            void finishCleanup().catch((error) => {
              console.error(`QA cleanup failed; preserved ${directory}: ${error.message}`);
              process.exitCode = 1;
            }),
          );
          return;
        }
        if (request.method !== "GET") return response.writeHead(405).end();
        if (url.pathname === "/latest.json") {
          const mode = (await readFile(modePath, "utf8")).trim();
          const manifest = mode === "valid" ? validManifestPath : tamperedManifestPath;
          response.writeHead(200, { "content-type": "application/json", "cache-control": "no-store" });
          return createReadStream(manifest).pipe(response);
        }
        if (url.pathname === "/update.exe") {
          response.writeHead(200, { "content-type": "application/octet-stream", "cache-control": "no-store" });
          return createReadStream(updateArtifact).pipe(response);
        }
        response.writeHead(url.pathname === "/health" ? 204 : 404).end();
      } catch (error) {
        if (!response.headersSent) response.writeHead(500, { "content-type": "text/plain" });
        response.end(error.message);
      }
    });
    await new Promise((accept, reject) => {
      server.once("error", reject);
      server.listen(0, "127.0.0.1", accept);
    });
    const port = server.address().port;
    const manifestUrl = `http://127.0.0.1:${port}/latest.json`;
    const artifactUrl = `http://127.0.0.1:${port}/update.exe`;

    const rustupCargo = await capture("rustup", ["which", "cargo"]);
    const env = { ...process.env, PATH: `${dirname(rustupCargo)};${process.env.PATH || ""}` };
    for (const name of Object.keys(env)) {
      if (name.startsWith("TAURI_")) delete env[name];
    }
    for (const name of [
      "APPLE_API_ISSUER",
      "APPLE_API_KEY",
      "APPLE_API_KEY_PATH",
      "APPLE_ID",
      "APPLE_PASSWORD",
      "APPLE_SIGNING_IDENTITY",
      "APPLE_TEAM_ID",
      "INSTALLER_SIGN_IDENTITY",
      "WINDOWS_CERTIFICATE_THUMBPRINT",
      "WINDOWS_TIMESTAMP_URL",
    ]) {
      delete env[name];
    }
    const privateKey = join(keys, "updater.key");
    const publicKey = `${privateKey}.pub`;
    await run("npx.cmd", ["tauri", "signer", "generate", "--ci", "--write-keys", privateKey], {
      env,
      quiet: true,
      onChild: (child) => (activeChild = child),
    });
    await chmod(privateKey, 0o600);
    const publicKeyContent = (await readFile(publicKey, "utf8")).trim();
    if (!publicKeyContent) throw new Error("Updater public key is empty");

    const signingEnv = { ...env, TAURI_SIGNING_PRIVATE_KEY: privateKey };
    const builds = {};
    for (const [name, version] of [["base", "0.1.0"], ["update", "0.1.1"]]) {
      const target = join(directory, "targets", name);
      const overlay = join(directory, `${name}.json`);
      await atomicWrite(overlay, `${JSON.stringify(qaOverlay(publicKeyContent, manifestUrl, version), null, 2)}\n`);
      await run(
        "npx.cmd",
        [
          "tauri",
          "build",
          "--ci",
          "--bundles",
          "nsis",
          "--target",
          targetTriple,
          "--config",
          `apps/${flavorName}/tauri.conf.json`,
          "--config",
          overlay,
        ],
        {
          env: qaBuildEnvironment(
            { ...signingEnv, CARGO_TARGET_DIR: target },
            dataRoot,
            flavorName,
          ),
          onChild: (child) => (activeChild = child),
        },
      );
      const bundle = join(target, targetTriple, "release", "bundle", "nsis");
      const builtBinary = join(target, targetTriple, "release", `${config.binaryName}.exe`);
      await regular(builtBinary);
      const preflight = (await capture(builtBinary, ["--windows-updater-qa-preflight"]))
        .split(/\r?\n/);
      if (preflight.length !== 2 || preflight[0] !== config.identifier || !samePath(preflight[1], dataRoot)) {
        throw new Error(`${name} build does not contain the exact Windows updater QA isolation`);
      }
      builds[name] = {
        setup: await oneFile(bundle, (file) => file.toLowerCase().endsWith("-setup.exe"), `${name} NSIS setup`),
      };
      await regular(`${builds[name].setup}.sig`);
      const info = await productInfo(builds[name].setup);
      if (info.productName !== config.productName || info.productVersion !== version) {
        throw new Error(`${name} NSIS metadata mismatch: ${info.productName} ${info.productVersion}`);
      }
    }
    baseSetup = builds.base.setup;
    updateArtifact = builds.update.setup;
    const signature = (await readFile(`${updateArtifact}.sig`, "utf8")).trim();
    await atomicWrite(validManifestPath, `${JSON.stringify(updateManifest(signature, artifactUrl), null, 2)}\n`);
    await atomicWrite(tamperedManifestPath, `${JSON.stringify(updateManifest(tamper(signature), artifactUrl), null, 2)}\n`);
    await writeFile(modePath, "tampered\n", { flag: "wx", mode: 0o600 });

    const control = {
      directory,
      flavor: flavorName,
      identifier: config.identifier,
      installDirectory,
      installUrl: `http://127.0.0.1:${port}/install/${stopToken}`,
      launchUrl: `http://127.0.0.1:${port}/launch/${stopToken}`,
      verifyUrl: `http://127.0.0.1:${port}/verify/${stopToken}`,
      stopUrl: `http://127.0.0.1:${port}/stop/${stopToken}`,
    };
    await atomicWrite(join(directory, "control.json"), `${JSON.stringify(control)}\n`);
    await atomicWrite(
      join(directory, "README.txt"),
      `Ephemeral ${config.productName} Windows x64 updater QA.\nProduction product identity is used only after fail-closed absence checks.\nArtifacts use a temporary updater key and no Authenticode certificate.\nAlways finish with the Stop command.\n`,
    );

    console.log(`Windows updater QA workspace: ${directory}`);
    console.log(`Flavor / production identity: ${flavorName} / ${config.identifier}`);
    console.log("Initial mode: tampered");
    console.log(`Update artifact SHA-256: ${await sha256(updateArtifact)}`);
    console.log(`Install: npm run qa:updater:windows -- install ${JSON.stringify(directory)}`);
    console.log(`Launch: npm run qa:updater:windows -- launch ${JSON.stringify(directory)}`);
    console.log("In the app, check/install once; the tampered signature must be rejected.");
    console.log(`Confirm unchanged: npm run qa:updater:windows -- verify ${JSON.stringify(directory)} 0.1.0`);
    console.log(`Valid mode: npm run qa:updater:windows -- mode ${JSON.stringify(directory)} valid`);
    console.log("In the app, check/install again; the valid update must install and restart.");
    console.log(`Confirm update: npm run qa:updater:windows -- verify ${JSON.stringify(directory)} 0.1.1`);
    console.log(`Stop: npm run qa:updater:windows -- stop ${JSON.stringify(directory)}`);
    console.log("This proves ephemeral updater signing only; it does not prove Authenticode signing.");
    console.log("The server stays active until Stop or Ctrl-C.");
  } catch (error) {
    try {
      await cleanup();
    } catch (cleanupError) {
      console.error(`QA cleanup failed; preserved ${directory}: ${cleanupError.message}`);
    }
    throw error;
  }
}

if (command === "--self-test" && process.argv.length === 3) await selfTest();
else if (command === "start") await start(process.argv[3]);
else if (command === "mode" && process.argv.length === 5) await setMode(process.argv[3], process.argv[4]);
else if (command === "install" && process.argv.length === 4) await install(process.argv[3]);
else if (command === "launch" && process.argv.length === 4) await launch(process.argv[3]);
else if (command === "verify" && process.argv.length === 5) await verify(process.argv[3], process.argv[4]);
else if (command === "stop" && process.argv.length === 4) await stop(process.argv[3]);
else throw new Error("Usage: prepare-updater-windows-qa.mjs [start <flavor> | install <workspace> | launch <workspace> | mode <workspace> <valid|tampered> | verify <workspace> <0.1.0|0.1.1> | stop <workspace> | --self-test]");
