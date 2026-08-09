import assert from "node:assert/strict";
import { createHash, createPublicKey, verify as verifyEd25519 } from "node:crypto";
import {
  chmodSync,
  constants,
  copyFileSync,
  existsSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  renameSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { basename, dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const targetRoot = join(root, "target");
const flavors = {
  "token-meter": {
    product: "TokenMeter",
    identifier: "local.tokenmeter.app",
    runtime: false,
  },
  "runtime-atlas": {
    product: "RuntimeAtlas",
    identifier: "com.kmg0308.runtimeatlas",
    runtime: true,
  },
  "dev-console": {
    product: "DevConsole",
    identifier: "com.kmg0308.devconsole",
    runtime: true,
  },
};
const targets = new Set(["universal-apple-darwin", "x86_64-pc-windows-msvc"]);
const releaseCredentialNames = new Set([
  "TAURI_SIGNING_PRIVATE_KEY",
  "TAURI_SIGNING_PRIVATE_KEY_PATH",
  "TAURI_SIGNING_PRIVATE_KEY_PASSWORD",
  "TAURI_PRIVATE_KEY",
  "TAURI_PRIVATE_KEY_PATH",
  "TAURI_PRIVATE_KEY_PASSWORD",
  "TAURI_KEY_PASSWORD",
  "APPLE_SIGNING_IDENTITY",
  "APPLE_API_ISSUER",
  "APPLE_API_KEY",
  "APPLE_API_KEY_PATH",
  "APPLE_ID",
  "APPLE_PASSWORD",
  "APPLE_TEAM_ID",
  "APPLE_CERTIFICATE",
  "APPLE_CERTIFICATE_PASSWORD",
  "APP_SIGN_IDENTITY",
  "INSTALLER_SIGN_IDENTITY",
  "WINDOWS_CERTIFICATE_THUMBPRINT",
  "WINDOWS_TIMESTAMP_URL",
]);

function releaseToolEnvironment(env, allowed = []) {
  const clean = { ...env };
  for (const name of releaseCredentialNames) delete clean[name];
  for (const name of allowed) {
    if (env[name] !== undefined) clean[name] = env[name];
  }
  return clean;
}

function updaterManifestName(flavor, target) {
  return `${flavor}-${target}.json`;
}

function required(env, name) {
  if (!env[name]?.trim()) throw new Error(`Missing required environment variable: ${name}`);
  return env[name].trim();
}

function httpsUrl(value, name) {
  let url;
  try {
    url = new URL(value);
  } catch {
    throw new Error(`${name} must be an HTTPS URL.`);
  }
  if (url.protocol !== "https:") throw new Error(`${name} must be an HTTPS URL.`);
  return value;
}

function validate(flavor, target, env, pathExists = existsSync) {
  if (!flavors[flavor]) throw new Error(`Invalid flavor: ${flavor}`);
  if (!targets.has(target)) throw new Error(`Invalid target: ${target}`);
  required(env, "TAURI_UPDATER_PUBLIC_KEY");
  const endpoint = httpsUrl(required(env, "TAURI_UPDATER_ENDPOINT"), "TAURI_UPDATER_ENDPOINT");
  if (decodeURIComponent(basename(new URL(endpoint).pathname)) !== updaterManifestName(flavor, target)) {
    throw new Error("TAURI_UPDATER_ENDPOINT must end with the selected flavor and target manifest name.");
  }
  httpsUrl(required(env, "TAURI_UPDATER_ARTIFACT_URL"), "TAURI_UPDATER_ARTIFACT_URL");
  required(env, "TAURI_SIGNING_PRIVATE_KEY");

  if (target === "universal-apple-darwin") {
    const identity = required(env, "APPLE_SIGNING_IDENTITY");
    if (!identity.startsWith("Developer ID Application: ")) {
      throw new Error("APPLE_SIGNING_IDENTITY must be a Developer ID Application identity.");
    }
    const apiPath = env.APPLE_API_KEY_PATH;
    const hasApiCredentials = env.APPLE_API_ISSUER?.trim()
      && env.APPLE_API_KEY?.trim()
      && apiPath?.trim()
      && isAbsolute(apiPath)
      && pathExists(apiPath);
    if (!hasApiCredentials) {
      throw new Error("Complete Apple API-key notarization credentials are required.");
    }
    if (flavor === "runtime-atlas"
      && !required(env, "INSTALLER_SIGN_IDENTITY").startsWith("Developer ID Installer: ")) {
      throw new Error("INSTALLER_SIGN_IDENTITY must be a Developer ID Installer identity.");
    }
  } else {
    for (const name of [
      "DEV_CONSOLE_WINDOWS_UPDATER_QA_ROOT",
      "DEV_CONSOLE_WINDOWS_UPDATER_QA_FLAVOR",
    ]) {
      if (env[name] !== undefined) {
        throw new Error(`${name} must not be set for a production Windows release.`);
      }
    }
    const thumbprint = required(env, "WINDOWS_CERTIFICATE_THUMBPRINT").replaceAll(" ", "");
    if (!/^[0-9A-F]{40}$/i.test(thumbprint)) {
      throw new Error("WINDOWS_CERTIFICATE_THUMBPRINT must contain exactly 40 hexadecimal characters.");
    }
    httpsUrl(required(env, "WINDOWS_TIMESTAMP_URL"), "WINDOWS_TIMESTAMP_URL");
  }
}

function overlay(target, env) {
  const bundle = { createUpdaterArtifacts: true };
  if (target === "x86_64-pc-windows-msvc") {
    bundle.windows = {
      digestAlgorithm: "sha256",
      certificateThumbprint: env.WINDOWS_CERTIFICATE_THUMBPRINT.replaceAll(" ", ""),
      timestampUrl: env.WINDOWS_TIMESTAMP_URL,
    };
  }
  return {
    build: { beforeBuildCommand: "" },
    bundle,
    plugins: {
      updater: {
        pubkey: env.TAURI_UPDATER_PUBLIC_KEY,
        endpoints: [env.TAURI_UPDATER_ENDPOINT],
      },
    },
  };
}

function command(flavor, target, overlayPath) {
  return {
    executable: process.execPath,
    args: [
      resolve(root, "node_modules/@tauri-apps/cli/tauri.js"),
      "build",
      "--config", resolve(root, `apps/${flavor}/tauri.conf.json`),
      "--config", overlayPath,
      "--target", target,
      "--bundles", target === "universal-apple-darwin" ? "app,dmg" : "nsis",
      "--ci",
    ],
  };
}

function releasePlan(flavor, target, version, outputRoot = targetRoot) {
  const app = flavors[flavor];
  if (!app || !targets.has(target)) throw new Error("Invalid release plan");
  const bundle = join(outputRoot, target, "release", "bundle");
  if (target === "universal-apple-darwin") {
    const directory = join(bundle, "macos");
    const updater = join(directory, `${app.product}.app.tar.gz`);
    return {
      ...app,
      flavor,
      target,
      version,
      app: join(directory, `${app.product}.app`),
      package: join(bundle, "dmg", `${app.product}_${version}_universal.dmg`),
      updater,
      signature: `${updater}.sig`,
      manifest: join(directory, updaterManifestName(flavor, target)),
      runtimePkg: flavor === "runtime-atlas"
        ? join(bundle, "pkg", `RuntimeAtlas-${version}.pkg`)
        : undefined,
    };
  }
  const directory = join(bundle, "nsis");
  const installer = join(directory, `${app.product}_${version}_x64-setup.exe`);
  return {
    ...app,
    flavor,
    target,
    version,
    installer,
    updater: installer,
    signature: `${installer}.sig`,
    manifest: join(directory, updaterManifestName(flavor, target)),
  };
}

function releaseOutputRoot(env) {
  return env.CARGO_TARGET_DIR?.trim()
    ? resolve(root, env.CARGO_TARGET_DIR)
    : targetRoot;
}

function releaseEnvironment(flavor, env) {
  if (env.CARGO_TARGET_DIR?.trim()) return env;
  return {
    ...env,
    CARGO_TARGET_DIR: join(targetRoot, "releases", flavor),
  };
}

function tauriReleaseEnvironment(plan, env) {
  const allowed = ["TAURI_SIGNING_PRIVATE_KEY", "TAURI_SIGNING_PRIVATE_KEY_PASSWORD"];
  if (plan.target === "universal-apple-darwin") {
    allowed.push("APPLE_SIGNING_IDENTITY");
    // Tauri does not sign macOS.files helpers; notarize RuntimeAtlas after installing its signed CLI.
    if (plan.flavor !== "runtime-atlas") {
      allowed.push("APPLE_API_ISSUER", "APPLE_API_KEY", "APPLE_API_KEY_PATH");
    }
  }
  return releaseToolEnvironment(env, allowed);
}

function releaseBuildSteps(plan, overlayPath, env) {
  const tauri = command(plan.flavor, plan.target, overlayPath);
  return [
    {
      name: "prebuild",
      executable: process.platform === "win32" ? "npm.cmd" : "npm",
      args: ["run", plan.runtime ? "build:runtime" : "build"],
      env: {
        ...releaseToolEnvironment(env),
        TAURI_ENV_TARGET_TRIPLE: plan.target,
        TAURI_ENV_DEBUG: "false",
      },
      error: "Release prebuild failed",
    },
    {
      name: "tauri",
      ...tauri,
      env: tauriReleaseEnvironment(plan, env),
      error: "Tauri release build failed",
    },
  ];
}

function clearReleaseOutputs(plan) {
  for (const path of new Set([
    plan.app,
    plan.package,
    plan.installer,
    plan.updater,
    plan.signature,
    plan.manifest,
    plan.runtimePkg,
  ].filter(Boolean))) {
    rmSync(path, { recursive: true, force: true });
  }
}

function updaterManifest(plan, artifactUrl, signature) {
  const url = httpsUrl(artifactUrl, "TAURI_UPDATER_ARTIFACT_URL");
  if (decodeURIComponent(basename(new URL(url).pathname)) !== basename(plan.updater)) {
    throw new Error("TAURI_UPDATER_ARTIFACT_URL must end with the exact updater artifact name.");
  }
  return {
    version: plan.version,
    url,
    signature,
  };
}

function decodeBase64(value, role) {
  if (!/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(value)) {
    throw new Error(`${role} is not valid base64`);
  }
  return Buffer.from(value, "base64");
}

function decodeUtf8Base64(value, role) {
  const decoded = decodeBase64(value, role);
  const text = decoded.toString("utf8");
  if (!Buffer.from(text, "utf8").equals(decoded)) throw new Error(`${role} is not valid UTF-8`);
  return text;
}

function verifyUpdaterSignatureData(data, encodedSignature, encodedPublicKey) {
  const publicLines = decodeUtf8Base64(encodedPublicKey, "Updater public key").trim().split(/\r?\n/);
  const signatureLines = decodeUtf8Base64(encodedSignature, "Updater signature").trim().split(/\r?\n/);
  if (publicLines.length !== 2 || signatureLines.length !== 4
    || !signatureLines[2].startsWith("trusted comment: ")) {
    throw new Error("Updater key or signature has invalid Minisign encoding");
  }

  const publicKey = decodeBase64(publicLines[1], "Minisign public key");
  const signature = decodeBase64(signatureLines[1], "Minisign signature");
  const globalSignature = decodeBase64(signatureLines[3], "Minisign global signature");
  if (publicKey.length !== 42 || signature.length !== 74 || globalSignature.length !== 64
    || !["Ed", "ED"].includes(publicKey.subarray(0, 2).toString("ascii"))
    || !["Ed", "ED"].includes(signature.subarray(0, 2).toString("ascii"))
    || !publicKey.subarray(2, 10).equals(signature.subarray(2, 10))) {
    throw new Error("Updater public key does not match the artifact signature");
  }

  const key = createPublicKey({
    key: Buffer.concat([
      Buffer.from("302a300506032b6570032100", "hex"),
      publicKey.subarray(10),
    ]),
    format: "der",
    type: "spki",
  });
  const algorithm = signature.subarray(0, 2).toString("ascii");
  const signedData = algorithm === "ED" ? createHash("blake2b512").update(data).digest() : data;
  const trustedComment = Buffer.from(signatureLines[2].slice("trusted comment: ".length));
  if (!verifyEd25519(null, signedData, key, signature.subarray(10))
    || !verifyEd25519(null, Buffer.concat([signature.subarray(10), trustedComment]), key, globalSignature)) {
    throw new Error("Updater artifact signature does not match TAURI_UPDATER_PUBLIC_KEY");
  }
}

function verifyUpdaterSignature(artifact, signature, publicKey) {
  verifyUpdaterSignatureData(
    readFileSync(artifact),
    readFileSync(signature, "utf8").trim(),
    publicKey.trim(),
  );
}

function requireFile(path, role) {
  const metadata = lstatSync(path);
  if (!metadata.isFile() || metadata.isSymbolicLink() || metadata.size === 0) {
    throw new Error(`${role} must be a nonempty regular file: ${path}`);
  }
}

function requireDirectory(path, role) {
  const metadata = lstatSync(path);
  if (!metadata.isDirectory() || metadata.isSymbolicLink()) {
    throw new Error(`${role} must be a regular directory: ${path}`);
  }
}

function run(executable, args, options = {}) {
  const result = spawnSync(executable, args, {
    cwd: options.cwd || root,
    env: options.env || releaseToolEnvironment(process.env),
    input: options.input,
    encoding: options.encoding,
    stdio: options.stdio,
  });
  if (result.error || result.status !== 0) {
    throw new Error(options.error || `${executable} failed`);
  }
  return result;
}

function plistValue(path, key) {
  return run("/usr/libexec/PlistBuddy", ["-c", `Print :${key}`, path], {
    encoding: "utf8",
    error: `Could not read ${key} from ${path}`,
  }).stdout.trim();
}

function verifyRuntimeAtlasAppExecutables(app, env, team, expectedCli) {
  const main = join(app, "Contents", "MacOS", "RuntimeAtlas");
  const cli = join(app, "Contents", "MacOS", "runtime-atlas");
  const supervisor = join(app, "Contents", "MacOS", "runtime-atlas-supervisor");
  const appHelper = join(app, "Contents", "Helpers", "runtime-atlas");
  for (const [path, role] of [
    [main, "RuntimeAtlas main executable"],
    [cli, "RuntimeAtlas bundled CLI"],
    [supervisor, "RuntimeAtlas supervisor"],
    [appHelper, "RuntimeAtlas public app helper"],
  ]) requireFile(path, role);
  if (!readFileSync(cli).equals(readFileSync(appHelper))
    || (expectedCli && !readFileSync(cli).equals(readFileSync(expectedCli)))) {
    throw new Error("RuntimeAtlas public app helper must exactly match the bundled CLI");
  }

  const mainArchs = run("/usr/bin/lipo", ["-archs", main], { encoding: "utf8" }).stdout.trim();
  for (const executable of [main, cli, supervisor, appHelper]) {
    if (run("/usr/bin/lipo", ["-archs", executable], { encoding: "utf8" }).stdout.trim() !== mainArchs) {
      throw new Error("RuntimeAtlas app helper architecture mismatch");
    }
    run("/usr/bin/codesign", ["--verify", "--strict", executable], {
      error: "RuntimeAtlas app helper code signature verification failed",
    });
    const details = run("/usr/bin/codesign", ["-dv", "--verbose=4", executable], {
      encoding: "utf8",
      error: "Could not inspect RuntimeAtlas app helper signature",
    }).stderr;
    if (!details.split("\n").includes(`Authority=${env.APPLE_SIGNING_IDENTITY}`)
      || details.match(/^TeamIdentifier=(.+)$/m)?.[1] !== team) {
      throw new Error("RuntimeAtlas app helper signing team mismatch");
    }
  }
  run("/bin/bash", [
    join(root, "scripts", "verify-runtime-atlas-macos-executables.sh"), cli, supervisor, appHelper,
  ], { error: "RuntimeAtlas app helper role verification failed" });
  return cli;
}

function verifyMacRelease(plan, env, temporary) {
  requireDirectory(plan.app, "macOS application");
  requireFile(plan.package, "macOS DMG");
  requireFile(plan.updater, "macOS updater artifact");
  requireFile(plan.signature, "macOS updater signature");

  const info = join(plan.app, "Contents", "Info.plist");
  if (plistValue(info, "CFBundleIdentifier") !== plan.identifier
    || plistValue(info, "CFBundleShortVersionString") !== plan.version
    || plistValue(info, "CFBundleExecutable") !== plan.product) {
    throw new Error("macOS application identity or version does not match the release plan");
  }

  run("/usr/bin/codesign", ["--verify", "--deep", "--strict", "--verbose=2", plan.app], {
    error: "macOS application code signature verification failed",
  });
  const signing = run("/usr/bin/codesign", ["-dv", "--verbose=4", plan.app], {
    encoding: "utf8",
    error: "Could not inspect macOS application code signature",
  }).stderr;
  if (!signing.split("\n").includes(`Authority=${env.APPLE_SIGNING_IDENTITY}`)
    || !signing.split("\n").includes(`Identifier=${plan.identifier}`)
    || !/^TeamIdentifier=(?!not set$).+/m.test(signing)) {
    throw new Error("macOS application is not signed by the requested Developer ID Application identity");
  }
  const team = signing.match(/^TeamIdentifier=(.+)$/m)?.[1];

  run("/usr/bin/codesign", ["--verify", "--strict", "--verbose=2", plan.package], {
    error: "macOS DMG code signature verification failed",
  });
  const dmgSigning = run("/usr/bin/codesign", ["-dv", "--verbose=4", plan.package], {
    encoding: "utf8",
    error: "Could not inspect macOS DMG code signature",
  }).stderr;
  if (!dmgSigning.split("\n").includes(`Authority=${env.APPLE_SIGNING_IDENTITY}`)
    || dmgSigning.match(/^TeamIdentifier=(.+)$/m)?.[1] !== team) {
    throw new Error("macOS DMG signing team does not match the application");
  }

  if (plan.flavor === "runtime-atlas") {
    const liveCli = verifyRuntimeAtlasAppExecutables(plan.app, env, team);
    const mount = join(temporary, "runtime-atlas-dmg");
    mkdirSync(mount, { mode: 0o700 });
    run("/usr/bin/hdiutil", ["attach", "-readonly", "-nobrowse", "-mountpoint", mount, plan.package], {
      error: "Could not mount the RuntimeAtlas DMG for verification",
    });
    try {
      verifyRuntimeAtlasAppExecutables(join(mount, "RuntimeAtlas.app"), env, team, liveCli);
    } finally {
      run("/usr/bin/hdiutil", ["detach", mount], {
        error: "Could not detach the RuntimeAtlas DMG after verification",
      });
    }
  }

  const archiveEntries = run("/usr/bin/tar", ["-tzf", plan.updater], {
    encoding: "utf8",
    error: "Could not inspect macOS updater archive",
  }).stdout.split("\n");
  const archiveRoot = `${plan.product}.app`;
  const archivePaths = [`${archiveRoot}/Contents/Info.plist`];
  if (plan.flavor === "runtime-atlas") {
    archivePaths.push(
      `${archiveRoot}/Contents/MacOS/RuntimeAtlas`,
      `${archiveRoot}/Contents/MacOS/runtime-atlas`,
      `${archiveRoot}/Contents/MacOS/runtime-atlas-supervisor`,
      `${archiveRoot}/Contents/Helpers/runtime-atlas`,
    );
  }
  if (archivePaths.some((path) => archiveEntries.filter((entry) => entry === path).length !== 1)) {
    throw new Error("macOS updater archive does not contain the exact application files");
  }
  run("/usr/bin/tar", ["-xzf", plan.updater, "-C", temporary, ...archivePaths], {
    error: "Could not read macOS updater application metadata",
  });
  const updaterInfo = join(temporary, archivePaths[0]);
  if (plistValue(updaterInfo, "CFBundleIdentifier") !== plan.identifier
    || plistValue(updaterInfo, "CFBundleShortVersionString") !== plan.version) {
    throw new Error("macOS updater identity or version does not match the release plan");
  }
  if (plan.flavor === "runtime-atlas") {
    verifyRuntimeAtlasAppExecutables(
      join(temporary, archiveRoot), env, team,
      join(plan.app, "Contents", "MacOS", "runtime-atlas"),
    );
  }

  for (const artifact of [plan.app, plan.package]) {
    run("/usr/bin/xcrun", ["stapler", "validate", artifact], {
      error: `Notarization ticket is not stapled to ${artifact}`,
    });
  }
  run("/usr/sbin/spctl", ["--assess", "--type", "execute", "--verbose=4", plan.app], {
    error: "Gatekeeper rejected the macOS application",
  });
  run("/usr/sbin/spctl", [
    "--assess", "--type", "open", "--context", "context:primary-signature", "--verbose=4", plan.package,
  ], { error: "Gatekeeper rejected the macOS DMG" });
}

function verifyWindowsRelease(plan, env) {
  requireFile(plan.installer, "Windows NSIS installer");
  requireFile(plan.signature, "Windows updater signature");
  const args = [
    "-NoProfile",
    "-NonInteractive",
    "-File", join(root, "scripts", "verify-windows-bundle.ps1"),
    "-ProductName", plan.product,
    "-MainBinaryName", plan.product,
    "-BundleIdentifier", plan.identifier,
    "-Version", plan.version,
    "-RuntimeFeature", String(plan.runtime),
    "-Target", plan.target,
    "-CertificateThumbprint", env.WINDOWS_CERTIFICATE_THUMBPRINT,
    "-InstallerPath", plan.installer,
  ];
  const executable = existsSync("C:\\Program Files\\PowerShell\\7\\pwsh.exe")
    ? "C:\\Program Files\\PowerShell\\7\\pwsh.exe"
    : "powershell.exe";
  run(executable, args, { stdio: "inherit", error: "Windows release verification failed" });
}

function notarizationArguments(pkg, env) {
  return [
    "notarytool", "submit", pkg, "--wait",
    "--issuer", env.APPLE_API_ISSUER,
    "--key-id", env.APPLE_API_KEY,
    "--key", env.APPLE_API_KEY_PATH,
  ];
}

function macNotarizationSteps(submission, artifact, env) {
  return [
    {
      name: "notarize",
      executable: "/usr/bin/xcrun",
      args: notarizationArguments(submission, env),
      error: `Notarization failed for ${artifact}`,
    },
    {
      name: "staple",
      executable: "/usr/bin/xcrun",
      args: ["stapler", "staple", artifact],
      error: `Notarization ticket could not be stapled to ${artifact}`,
    },
  ];
}

function notarizeAndStapleMacArtifact(artifact, env, temporary) {
  let submission = artifact;
  if (artifact.endsWith(".app")) {
    submission = join(temporary, `${basename(artifact)}.zip`);
    rmSync(submission, { force: true });
    run("/usr/bin/ditto", ["-c", "-k", "--keepParent", artifact, submission], {
      error: "Could not archive corrected RuntimeAtlas app for notarization",
    });
  }
  for (const step of macNotarizationSteps(submission, artifact, env)) {
    run(step.executable, step.args, { error: step.error });
  }
}

function macReleasePostBuildPlan(plan, env) {
  return [
    ...macNotarizationSteps(plan.package, plan.package, env),
    { name: "verify" },
  ];
}

function finalizeMacRelease(plan, env, temporary) {
  for (const step of macReleasePostBuildPlan(plan, env)) {
    if (step.name === "verify") {
      verifyMacRelease(plan, env, temporary);
    } else {
      run(step.executable, step.args, { error: step.error });
    }
  }
}

function runtimeAtlasDmgArguments(plan, env) {
  const args = [
    "--volname", plan.product,
    "--icon", `${plan.product}.app`, "180", "170",
    "--app-drop-link", "480", "170",
    "--window-size", "660", "400",
    "--hide-extension", `${plan.product}.app`,
  ];
  const icon = join(dirname(plan.package), "icon.icns");
  if (existsSync(icon)) args.push("--volicon", icon);
  if (env.CI === "true") args.push("--skip-jenkins");
  args.push(basename(plan.package), basename(plan.app));
  return args;
}

function runtimeAtlasRepairPlan(plan, env, temporary) {
  const cli = join(plan.app, "Contents", "MacOS", "runtime-atlas");
  const helper = join(plan.app, "Contents", "Helpers", "runtime-atlas");
  const appArchive = join(temporary, `${basename(plan.app)}.zip`);
  const dmgScript = join(dirname(plan.package), "bundle_dmg.sh");
  const intermediateDmg = join(dirname(plan.app), basename(plan.package));
  return {
    cli,
    helper,
    appSign: ["/usr/bin/codesign", [
      "--force", "--sign", env.APPLE_SIGNING_IDENTITY, "--options", "runtime", "--timestamp", plan.app,
    ]],
    appArchive: ["/usr/bin/ditto", ["-c", "-k", "--keepParent", plan.app, appArchive]],
    appNotarize: ["/usr/bin/xcrun", notarizationArguments(appArchive, env)],
    appStaple: ["/usr/bin/xcrun", ["stapler", "staple", plan.app]],
    dmgScript,
    dmg: ["/bin/bash", [dmgScript, ...runtimeAtlasDmgArguments(plan, env)]],
    intermediateDmg,
    dmgSign: ["/usr/bin/codesign", [
      "--force", "--sign", env.APPLE_SIGNING_IDENTITY, "--timestamp", plan.package,
    ]],
    updater: ["/usr/bin/tar", [
      "-czf", plan.updater, "-C", dirname(plan.app), basename(plan.app),
    ]],
    updaterSign: [process.execPath, [
      resolve(root, "node_modules/@tauri-apps/cli/tauri.js"), "signer", "sign", plan.updater,
    ]],
  };
}

function repairRuntimeAtlasMacRelease(plan, env, temporary) {
  const repair = runtimeAtlasRepairPlan(plan, env, temporary);
  requireFile(repair.cli, "RuntimeAtlas bundled CLI");
  requireFile(repair.helper, "RuntimeAtlas public app helper");
  run("/usr/bin/codesign", ["--verify", "--strict", repair.cli], {
    error: "Tauri did not sign the RuntimeAtlas bundled CLI",
  });
  const cliSigning = run("/usr/bin/codesign", ["-dv", "--verbose=4", repair.cli], {
    encoding: "utf8",
    error: "Could not inspect RuntimeAtlas bundled CLI signature",
  }).stderr;
  if (!cliSigning.split("\n").includes(`Authority=${env.APPLE_SIGNING_IDENTITY}`)) {
    throw new Error("Tauri did not sign the RuntimeAtlas bundled CLI with the requested identity");
  }

  copyFileSync(repair.cli, repair.helper);
  chmodSync(repair.helper, 0o755);
  run(...repair.appSign, { error: "Could not re-sign RuntimeAtlas after installing its public app helper" });
  notarizeAndStapleMacArtifact(plan.app, env, temporary);

  requireFile(repair.dmgScript, "Tauri DMG builder");
  rmSync(plan.package, { force: true });
  rmSync(repair.intermediateDmg, { force: true });
  run(...repair.dmg, {
    cwd: dirname(plan.app),
    env: releaseToolEnvironment(env),
    error: "Could not rebuild RuntimeAtlas DMG from the corrected app",
  });
  renameSync(repair.intermediateDmg, plan.package);
  run(...repair.dmgSign, { error: "Could not sign the corrected RuntimeAtlas DMG" });

  rmSync(plan.updater, { force: true });
  rmSync(plan.signature, { force: true });
  run(...repair.updater, {
    env: { ...releaseToolEnvironment(env), COPYFILE_DISABLE: "1" },
    error: "Could not rebuild the RuntimeAtlas updater from the corrected app",
  });
  run(...repair.updaterSign, {
    env: releaseToolEnvironment(env, [
      "TAURI_SIGNING_PRIVATE_KEY", "TAURI_SIGNING_PRIVATE_KEY_PASSWORD",
    ]),
    error: "Could not sign the corrected RuntimeAtlas updater",
  });
}

function buildRuntimeAtlasPkg(plan, env, temporary) {
  const stagedPkg = join(temporary, `RuntimeAtlas-${plan.version}.pkg`);
  run("/bin/bash", [join(root, "scripts", "package-runtime-atlas-macos.sh"), plan.app, stagedPkg], {
    env: {
      ...releaseToolEnvironment(env),
      APP_SIGN_IDENTITY: env.APPLE_SIGNING_IDENTITY,
      INSTALLER_SIGN_IDENTITY: env.INSTALLER_SIGN_IDENTITY,
    },
    stdio: "inherit",
    error: "RuntimeAtlas PKG build failed",
  });
  run("/usr/bin/xcrun", notarizationArguments(stagedPkg, env), {
    error: "RuntimeAtlas PKG notarization failed",
  });
  run("/usr/bin/xcrun", ["stapler", "staple", stagedPkg], {
    error: "RuntimeAtlas PKG stapling failed",
  });
  run("/bin/bash", [
    join(root, "scripts", "verify-runtime-atlas-macos-package.sh"), stagedPkg, "signed", "stapled",
  ], { stdio: "inherit", error: "RuntimeAtlas PKG verification failed" });
  mkdirSync(dirname(plan.runtimePkg), { recursive: true });
  copyFileSync(stagedPkg, plan.runtimePkg, constants.COPYFILE_EXCL);
  requireFile(plan.runtimePkg, "RuntimeAtlas PKG");
}

function writeManifest(plan, env) {
  const signature = readFileSync(plan.signature, "utf8").trim();
  if (!signature) throw new Error("Updater signature is empty");
  verifyUpdaterSignature(plan.updater, plan.signature, env.TAURI_UPDATER_PUBLIC_KEY);
  const manifest = updaterManifest(plan, env.TAURI_UPDATER_ARTIFACT_URL, signature);
  writeFileSync(plan.manifest, `${JSON.stringify(manifest, null, 2)}\n`);
  requireFile(plan.manifest, "Updater manifest");
}

function workspaceVersion() {
  const version = JSON.parse(readFileSync(join(root, "src-tauri", "tauri.conf.json"), "utf8")).version;
  if (typeof version !== "string" || !/^\d+\.\d+\.\d+$/.test(version)) {
    throw new Error("src-tauri/tauri.conf.json must contain a numeric semantic version");
  }
  return version;
}

function signatureSelfTest() {
  const temporary = mkdtempSync(join(tmpdir(), "dev-console-signature-test-"));
  chmodSync(temporary, 0o700);
  const tauri = resolve(root, "node_modules/@tauri-apps/cli/tauri.js");
  const artifact = join(temporary, "artifact.bin");
  const privateKey = join(temporary, "updater.key");
  const wrongPrivateKey = join(temporary, "wrong.key");
  try {
    writeFileSync(artifact, "signed updater fixture\n");
    for (const key of [privateKey, wrongPrivateKey]) {
      run(process.execPath, [tauri, "signer", "generate", "--ci", "--write-keys", key], {
        error: "Could not generate an updater signature test key",
      });
      chmodSync(key, 0o600);
    }
    run(process.execPath, [tauri, "signer", "sign", "--private-key-path", privateKey, artifact], {
      env: { ...releaseToolEnvironment(process.env), TAURI_SIGNING_PRIVATE_KEY_PASSWORD: "" },
      error: "Could not sign the updater signature test artifact",
    });
    const signature = `${artifact}.sig`;
    verifyUpdaterSignature(artifact, signature, readFileSync(`${privateKey}.pub`, "utf8"));
    assert.throws(
      () => verifyUpdaterSignature(artifact, signature, readFileSync(`${wrongPrivateKey}.pub`, "utf8")),
      /does not match/,
    );
    process.stdout.write("release signature integration self-test passed\n");
  } finally {
    rmSync(temporary, { recursive: true, force: true });
  }
}

function selfTest() {
  const successOutput = "release config self-test passed\n";
  const common = {
    TAURI_UPDATER_PUBLIC_KEY: "public-key",
    TAURI_UPDATER_ENDPOINT: "https://downloads.example.test/token-meter/token-meter-universal-apple-darwin.json",
    TAURI_UPDATER_ARTIFACT_URL: "https://downloads.example.test/token-meter/TokenMeter.app.tar.gz",
    TAURI_SIGNING_PRIVATE_KEY: "private-key-must-not-leak",
  };
  const mac = {
    ...common,
    APPLE_SIGNING_IDENTITY: "Developer ID Application: Example (TEAMID1234)",
    APPLE_API_ISSUER: "issuer",
    APPLE_API_KEY: "key-id",
    APPLE_API_KEY_PATH: "/private/key.p8",
    INSTALLER_SIGN_IDENTITY: "Developer ID Installer: Example (TEAMID1234)",
  };
  const windows = {
    ...common,
    TAURI_UPDATER_ENDPOINT: "https://downloads.example.test/token-meter/token-meter-x86_64-pc-windows-msvc.json",
    TAURI_UPDATER_ARTIFACT_URL: "https://downloads.example.test/token-meter/TokenMeter_0.1.0_x64-setup.exe",
    WINDOWS_CERTIFICATE_THUMBPRINT: "0123456789ABCDEF0123456789ABCDEF01234567",
    WINDOWS_TIMESTAMP_URL: "https://timestamp.example.test",
  };

  assert.throws(() => validate("token-meter", "universal-apple-darwin", {}, () => true), /TAURI_UPDATER_PUBLIC_KEY/);
  assert.throws(() => validate("other", "universal-apple-darwin", mac, () => true), /Invalid flavor/);
  assert.throws(() => validate("token-meter", "other", mac, () => true), /Invalid target/);
  assert.throws(() => validate("token-meter", "universal-apple-darwin", { ...mac, TAURI_UPDATER_ENDPOINT: "http://example.test" }, () => true), /HTTPS URL/);
  assert.throws(
    () => validate("runtime-atlas", "universal-apple-darwin", mac, () => true),
    /selected flavor and target/,
  );
  assert.throws(
    () => validate("token-meter", "x86_64-pc-windows-msvc", { ...windows, TAURI_UPDATER_ENDPOINT: common.TAURI_UPDATER_ENDPOINT }),
    /selected flavor and target/,
  );
  assert.throws(() => validate("token-meter", "universal-apple-darwin", { ...mac, TAURI_UPDATER_ARTIFACT_URL: "http://example.test/app" }, () => true), /HTTPS URL/);
  assert.throws(() => validate("token-meter", "universal-apple-darwin", { ...mac, APPLE_SIGNING_IDENTITY: "-" }, () => true), /Developer ID Application/);
  assert.throws(() => validate("token-meter", "universal-apple-darwin", { ...mac, APPLE_SIGNING_IDENTITY: "Apple Development: Example" }, () => true), /Developer ID Application/);
  assert.throws(() => validate("runtime-atlas", "universal-apple-darwin", {
    ...mac,
    TAURI_UPDATER_ENDPOINT: "https://downloads.example.test/runtime-atlas/runtime-atlas-universal-apple-darwin.json",
    INSTALLER_SIGN_IDENTITY: "-",
  }, () => true), /Developer ID Installer/);
  assert.throws(() => validate("token-meter", "universal-apple-darwin", { ...mac, APPLE_API_KEY_PATH: "relative.p8" }, () => true), /notarization credentials/);
  assert.throws(() => validate("token-meter", "universal-apple-darwin", mac, () => false), /notarization credentials/);
  assert.throws(() => validate("token-meter", "universal-apple-darwin", {
    ...mac,
    APPLE_API_ISSUER: undefined,
    APPLE_API_KEY: undefined,
    APPLE_API_KEY_PATH: undefined,
    APPLE_ID: "release@example.test",
    APPLE_PASSWORD: "must-not-be-used",
    APPLE_TEAM_ID: "TEAMID1234",
  }, () => true), /API-key notarization credentials/);
  assert.throws(() => validate("token-meter", "x86_64-pc-windows-msvc", { ...windows, WINDOWS_CERTIFICATE_THUMBPRINT: "bad" }), /40 hexadecimal/);
  for (const name of [
    "DEV_CONSOLE_WINDOWS_UPDATER_QA_ROOT",
    "DEV_CONSOLE_WINDOWS_UPDATER_QA_FLAVOR",
  ]) {
    for (const value of ["qa", ""]) {
      assert.throws(
        () => validate("token-meter", "x86_64-pc-windows-msvc", { ...windows, [name]: value }),
        new RegExp(name),
      );
    }
  }
  validate("token-meter", "universal-apple-darwin", mac, () => true);
  validate("runtime-atlas", "universal-apple-darwin", {
    ...mac,
    TAURI_UPDATER_ENDPOINT: "https://downloads.example.test/runtime-atlas/runtime-atlas-universal-apple-darwin.json",
  }, () => true);
  validate("dev-console", "universal-apple-darwin", {
    ...mac,
    TAURI_UPDATER_ENDPOINT: "https://downloads.example.test/dev-console/dev-console-universal-apple-darwin.json",
  }, () => true);
  validate("runtime-atlas", "x86_64-pc-windows-msvc", {
    ...windows,
    TAURI_UPDATER_ENDPOINT: "https://downloads.example.test/runtime-atlas/runtime-atlas-x86_64-pc-windows-msvc.json",
  });

  const testOutput = resolve(root, "release-self-test-output");
  const macPlan = releasePlan("token-meter", "universal-apple-darwin", "0.1.0", testOutput);
  assert.deepEqual(macPlan, {
    product: "TokenMeter",
    identifier: "local.tokenmeter.app",
    runtime: false,
    flavor: "token-meter",
    target: "universal-apple-darwin",
    version: "0.1.0",
    app: join(testOutput, "universal-apple-darwin", "release", "bundle", "macos", "TokenMeter.app"),
    package: join(testOutput, "universal-apple-darwin", "release", "bundle", "dmg", "TokenMeter_0.1.0_universal.dmg"),
    updater: join(testOutput, "universal-apple-darwin", "release", "bundle", "macos", "TokenMeter.app.tar.gz"),
    signature: join(testOutput, "universal-apple-darwin", "release", "bundle", "macos", "TokenMeter.app.tar.gz.sig"),
    manifest: join(testOutput, "universal-apple-darwin", "release", "bundle", "macos", "token-meter-universal-apple-darwin.json"),
    runtimePkg: undefined,
  });
  const windowsPlan = releasePlan("dev-console", "x86_64-pc-windows-msvc", "0.1.0", testOutput);
  assert.equal(windowsPlan.installer, join(testOutput, "x86_64-pc-windows-msvc", "release", "bundle", "nsis", "DevConsole_0.1.0_x64-setup.exe"));
  assert.equal(windowsPlan.updater, windowsPlan.installer);
  assert.equal(windowsPlan.signature, `${windowsPlan.updater}.sig`);
  assert.equal(windowsPlan.manifest, join(testOutput, "x86_64-pc-windows-msvc", "release", "bundle", "nsis", "dev-console-x86_64-pc-windows-msvc.json"));
  assert.deepEqual(updaterManifest(windowsPlan, "https://downloads.example.test/dev-console/DevConsole_0.1.0_x64-setup.exe", "windows-signature"), {
    version: "0.1.0",
    url: "https://downloads.example.test/dev-console/DevConsole_0.1.0_x64-setup.exe",
    signature: "windows-signature",
  });
  assert.deepEqual(updaterManifest(macPlan, common.TAURI_UPDATER_ARTIFACT_URL, "artifact-signature"), {
    version: "0.1.0",
    url: "https://downloads.example.test/token-meter/TokenMeter.app.tar.gz",
    signature: "artifact-signature",
  });
  assert.throws(() => updaterManifest(macPlan, "https://downloads.example.test/wrong.tar.gz", "artifact-signature"), /exact updater artifact name/);
  assert.equal(releasePlan("runtime-atlas", "universal-apple-darwin", "0.1.0", testOutput).runtimePkg,
    join(testOutput, "universal-apple-darwin", "release", "bundle", "pkg", "RuntimeAtlas-0.1.0.pkg"));
  const runtimeMacPlan = releasePlan("runtime-atlas", "universal-apple-darwin", "0.1.0", testOutput);
  assert.deepEqual(runtimeAtlasDmgArguments(runtimeMacPlan, { CI: "true" }), [
    "--volname", "RuntimeAtlas",
    "--icon", "RuntimeAtlas.app", "180", "170",
    "--app-drop-link", "480", "170",
    "--window-size", "660", "400",
    "--hide-extension", "RuntimeAtlas.app",
    "--skip-jenkins", "RuntimeAtlas_0.1.0_universal.dmg", "RuntimeAtlas.app",
  ]);
  const tauriMacEnv = tauriReleaseEnvironment(runtimeMacPlan, mac);
  assert.equal(tauriMacEnv.APPLE_SIGNING_IDENTITY, mac.APPLE_SIGNING_IDENTITY);
  assert.equal(tauriMacEnv.TAURI_SIGNING_PRIVATE_KEY, common.TAURI_SIGNING_PRIVATE_KEY);
  for (const name of [
    "APPLE_API_ISSUER", "APPLE_API_KEY", "APPLE_API_KEY_PATH",
    "APPLE_ID", "APPLE_PASSWORD", "APPLE_TEAM_ID",
  ]) assert.equal(tauriMacEnv[name], undefined);
  const tokenMeterTauriEnv = tauriReleaseEnvironment(macPlan, mac);
  for (const name of [
    "APPLE_SIGNING_IDENTITY", "APPLE_API_ISSUER", "APPLE_API_KEY", "APPLE_API_KEY_PATH",
    "TAURI_SIGNING_PRIVATE_KEY",
  ]) assert.equal(tokenMeterTauriEnv[name], mac[name]);
  assert.equal(tokenMeterTauriEnv.INSTALLER_SIGN_IDENTITY, undefined);
  const scrubbed = releaseToolEnvironment({
    KEEP: "yes",
    TAURI_SIGNING_PRIVATE_KEY: "private",
    TAURI_SIGNING_PRIVATE_KEY_PATH: "/private/updater.key",
    TAURI_SIGNING_PRIVATE_KEY_PASSWORD: "password",
    TAURI_PRIVATE_KEY: "deprecated-private",
    TAURI_PRIVATE_KEY_PATH: "/private/deprecated-updater.key",
    TAURI_PRIVATE_KEY_PASSWORD: "deprecated-password",
    TAURI_KEY_PASSWORD: "legacy-password",
    APPLE_API_KEY_PATH: "/private/key.p8",
    APPLE_PASSWORD: "password",
    WINDOWS_CERTIFICATE_THUMBPRINT: windows.WINDOWS_CERTIFICATE_THUMBPRINT,
  });
  assert.deepEqual(scrubbed, { KEEP: "yes" });
  assert.deepEqual(
    releaseToolEnvironment({ ...scrubbed, TAURI_SIGNING_PRIVATE_KEY: "private" }, [
      "TAURI_SIGNING_PRIVATE_KEY",
    ]),
    { KEEP: "yes", TAURI_SIGNING_PRIVATE_KEY: "private" },
  );
  const repair = runtimeAtlasRepairPlan(runtimeMacPlan, mac, "/private/release");
  assert.deepEqual([
    repair.appSign[0], repair.appArchive[0], repair.appNotarize[0], repair.appStaple[0],
    repair.dmg[0], repair.dmgSign[0], repair.updater[0], repair.updaterSign[0],
  ], [
    "/usr/bin/codesign", "/usr/bin/ditto", "/usr/bin/xcrun", "/usr/bin/xcrun",
    "/bin/bash", "/usr/bin/codesign", "/usr/bin/tar", process.execPath,
  ]);
  assert.equal(repair.appSign[1].at(-1), runtimeMacPlan.app);
  assert.equal(repair.appArchive[1].at(-2), runtimeMacPlan.app);
  assert.equal(repair.appArchive[1].at(-1), "/private/release/RuntimeAtlas.app.zip");
  assert.deepEqual(repair.appNotarize[1].slice(0, 3), [
    "notarytool", "submit", "/private/release/RuntimeAtlas.app.zip",
  ]);
  assert(!repair.appNotarize[1].includes("--password"));
  assert(!repair.appNotarize[1].includes("--apple-id"));
  assert.deepEqual(repair.appStaple[1], ["stapler", "staple", runtimeMacPlan.app]);
  assert.equal(repair.dmg[1].at(-2), basename(runtimeMacPlan.package));
  assert.equal(repair.dmg[1].at(-1), basename(runtimeMacPlan.app));
  assert.equal(repair.dmgSign[1].at(-1), runtimeMacPlan.package);
  assert.deepEqual(repair.updater[1], [
    "-czf", runtimeMacPlan.updater, "-C", dirname(runtimeMacPlan.app), basename(runtimeMacPlan.app),
  ]);
  assert.equal(repair.updaterSign[1].at(-1), runtimeMacPlan.updater);
  for (const flavor of Object.keys(flavors)) {
    const plan = releasePlan(flavor, "universal-apple-darwin", "0.1.0", testOutput);
    const steps = macReleasePostBuildPlan(plan, mac);
    assert.deepEqual(steps.map(({ name }) => name), ["notarize", "staple", "verify"]);
    assert.deepEqual(steps[0].args, [
      "notarytool", "submit", plan.package, "--wait",
      "--issuer", "issuer", "--key-id", "key-id", "--key", "/private/key.p8",
    ]);
    assert.deepEqual(steps[1].args, ["stapler", "staple", plan.package]);
  }
  assert.equal(releaseOutputRoot({}), targetRoot);
  assert.equal(releaseOutputRoot({ CARGO_TARGET_DIR: "custom-target" }), join(root, "custom-target"));
  const absoluteTarget = resolve(root, "absolute-custom-target");
  assert.equal(releaseOutputRoot({ CARGO_TARGET_DIR: absoluteTarget }), absoluteTarget);
  const isolatedReleaseRoots = Object.keys(flavors).map((flavor) =>
    releaseOutputRoot(releaseEnvironment(flavor, {})));
  assert.deepEqual(isolatedReleaseRoots, Object.keys(flavors).map((flavor) =>
    join(targetRoot, "releases", flavor)));
  assert.equal(new Set(isolatedReleaseRoots).size, 3);
  const explicitTargetEnv = { CARGO_TARGET_DIR: "custom-target" };
  assert.equal(releaseEnvironment("dev-console", explicitTargetEnv), explicitTargetEnv);

  const minisignPublic = Buffer.from(
    "untrusted comment: minisign public key E7620F1842B4E81F\n"
      + "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3",
  ).toString("base64");
  const minisignSignature = Buffer.from(
    "untrusted comment: signature from minisign secret key\n"
      + "RUQf6LRCGA9i559r3g7V1qNyJDApGip8MfqcadIgT9CuhV3EMhHoN1mGTkUidF/z7SrlQgXdy8ofjb7bNJJylDOocrCo8KLzZwo=\n"
      + "trusted comment: timestamp:1556193335\tfile:test\n"
      + "y/rUw2y8/hOUYjZU71eHp/Wo1KZ40fGy2VJEDl34XMJM+TX48Ss/17u3IvIfbVR1FkZZSNCisQbuQY+bHwhEBg==",
  ).toString("base64");
  assert.doesNotThrow(() => verifyUpdaterSignatureData(Buffer.from("test"), minisignSignature, minisignPublic));
  assert.throws(
    () => verifyUpdaterSignatureData(Buffer.from("tampered"), minisignSignature, minisignPublic),
    /does not match/,
  );
  const wrongPublicBytes = Buffer.from("RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3", "base64");
  wrongPublicBytes[wrongPublicBytes.length - 1] ^= 1;
  const wrongPublic = Buffer.from(
    `untrusted comment: wrong public key\n${wrongPublicBytes.toString("base64")}`,
  ).toString("base64");
  assert.throws(
    () => verifyUpdaterSignatureData(Buffer.from("test"), minisignSignature, wrongPublic),
    /does not match/,
  );

  const devConsoleWindows = {
    ...windows,
    TAURI_UPDATER_ENDPOINT: "https://downloads.example.test/dev-console/dev-console-x86_64-pc-windows-msvc.json",
    TAURI_UPDATER_ARTIFACT_URL: "https://downloads.example.test/dev-console/DevConsole_0.1.0_x64-setup.exe",
  };
  const releaseOverlay = overlay("x86_64-pc-windows-msvc", devConsoleWindows);
  assert.equal(releaseOverlay.build.beforeBuildCommand, "");
  assert.equal(releaseOverlay.bundle.createUpdaterArtifacts, true);
  assert.equal(releaseOverlay.plugins.updater.endpoints[0], devConsoleWindows.TAURI_UPDATER_ENDPOINT);
  const invocation = command("dev-console", "x86_64-pc-windows-msvc", "/private/release/overlay.json");
  assert(!invocation.args.includes("--no-sign"));
  assert(!JSON.stringify({ releaseOverlay, invocation, output: successOutput }).includes(common.TAURI_SIGNING_PRIVATE_KEY));
  const buildEnv = { ...mac, CARGO_TARGET_DIR: "/private/release/target" };
  const buildSteps = releaseBuildSteps(runtimeMacPlan, "/private/release/overlay.json", buildEnv);
  assert.deepEqual(buildSteps.map(({ name }) => name), ["prebuild", "tauri"]);
  assert.deepEqual(buildSteps[0].args, ["run", "build:runtime"]);
  assert.equal(buildSteps[0].env.CARGO_TARGET_DIR, buildEnv.CARGO_TARGET_DIR);
  assert.equal(buildSteps[0].env.TAURI_ENV_TARGET_TRIPLE, runtimeMacPlan.target);
  assert.equal(buildSteps[0].env.TAURI_ENV_DEBUG, "false");
  for (const name of releaseCredentialNames) assert.equal(buildSteps[0].env[name], undefined);
  assert.equal(buildSteps[1].env.TAURI_SIGNING_PRIVATE_KEY, common.TAURI_SIGNING_PRIVATE_KEY);
  assert.deepEqual(releaseBuildSteps(macPlan, "/private/release/overlay.json", mac)[0].args, ["run", "build"]);
  process.stdout.write(successOutput);
}

function main() {
  if (process.argv.length === 3 && process.argv[2] === "--self-test") return selfTest();
  if (process.argv.length === 3 && process.argv[2] === "--self-test-signature") return signatureSelfTest();
  if (process.argv.length !== 4) {
    throw new Error("Usage: build-release.mjs <token-meter|runtime-atlas|dev-console> <universal-apple-darwin|x86_64-pc-windows-msvc>");
  }

  const [, , flavor, target] = process.argv;
  const releaseEnv = releaseEnvironment(flavor, process.env);
  validate(flavor, target, releaseEnv);
  const temporary = mkdtempSync(join(tmpdir(), "dev-console-release-"));
  chmodSync(temporary, 0o700);
  const overlayPath = join(temporary, "tauri.release.conf.json");
  try {
    const plan = releasePlan(flavor, target, workspaceVersion(), releaseOutputRoot(releaseEnv));
    updaterManifest(plan, releaseEnv.TAURI_UPDATER_ARTIFACT_URL, "preflight");
    clearReleaseOutputs(plan);
    writeFileSync(overlayPath, `${JSON.stringify(overlay(target, releaseEnv))}\n`, { mode: 0o600 });
    chmodSync(overlayPath, 0o600);
    for (const step of releaseBuildSteps(plan, overlayPath, releaseEnv)) {
      run(step.executable, step.args, {
        env: step.env,
        stdio: "inherit",
        error: step.error,
      });
    }

    if (target === "universal-apple-darwin") {
      if (plan.flavor === "runtime-atlas") {
        repairRuntimeAtlasMacRelease(plan, releaseEnv, temporary);
      }
      finalizeMacRelease(plan, releaseEnv, temporary);
      if (plan.runtimePkg) buildRuntimeAtlasPkg(plan, releaseEnv, temporary);
    } else {
      verifyWindowsRelease(plan, releaseEnv);
    }
    writeManifest(plan, releaseEnv);
    process.stdout.write(`Verified updater artifact: ${plan.updater}\nUpdater manifest: ${plan.manifest}\n`);
    if (plan.runtimePkg) process.stdout.write(`Verified RuntimeAtlas PKG: ${plan.runtimePkg}\n`);
  } finally {
    rmSync(temporary, { recursive: true, force: true });
  }
}

main();
