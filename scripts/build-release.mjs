import assert from "node:assert/strict";
import { existsSync, mkdtempSync, rmSync, writeFileSync, chmodSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const flavors = new Set(["token-meter", "runtime-atlas", "dev-console"]);
const targets = new Set(["universal-apple-darwin", "x86_64-pc-windows-msvc"]);

function required(env, name) {
  if (!env[name]?.trim()) throw new Error(`Missing required environment variable: ${name}`);
  return env[name];
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
  if (!flavors.has(flavor)) throw new Error(`Invalid flavor: ${flavor}`);
  if (!targets.has(target)) throw new Error(`Invalid target: ${target}`);
  required(env, "TAURI_UPDATER_PUBLIC_KEY");
  httpsUrl(required(env, "TAURI_UPDATER_ENDPOINT"), "TAURI_UPDATER_ENDPOINT");
  required(env, "TAURI_SIGNING_PRIVATE_KEY");

  if (target === "universal-apple-darwin") {
    required(env, "APPLE_SIGNING_IDENTITY");
    const apiPath = env.APPLE_API_KEY_PATH;
    const hasApiCredentials = env.APPLE_API_ISSUER?.trim()
      && env.APPLE_API_KEY?.trim()
      && apiPath?.trim()
      && isAbsolute(apiPath)
      && pathExists(apiPath);
    const hasAppleIdCredentials = env.APPLE_ID?.trim()
      && env.APPLE_PASSWORD?.trim()
      && env.APPLE_TEAM_ID?.trim();
    if (!hasApiCredentials && !hasAppleIdCredentials) {
      throw new Error("Complete Apple API-key or Apple ID notarization credentials are required.");
    }
  } else {
    required(env, "WINDOWS_CERTIFICATE_THUMBPRINT");
    httpsUrl(required(env, "WINDOWS_TIMESTAMP_URL"), "WINDOWS_TIMESTAMP_URL");
  }
}

function overlay(target, env) {
  const bundle = { createUpdaterArtifacts: true };
  if (target === "x86_64-pc-windows-msvc") {
    bundle.windows = {
      digestAlgorithm: "sha256",
      certificateThumbprint: env.WINDOWS_CERTIFICATE_THUMBPRINT,
      timestampUrl: env.WINDOWS_TIMESTAMP_URL,
    };
  }
  return {
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

function selfTest() {
  const successOutput = "release config self-test passed\n";
  const common = {
    TAURI_UPDATER_PUBLIC_KEY: "public-key",
    TAURI_UPDATER_ENDPOINT: "https://downloads.example.test/token-meter/latest.json",
    TAURI_SIGNING_PRIVATE_KEY: "private-key-must-not-leak",
  };
  const mac = {
    ...common,
    APPLE_SIGNING_IDENTITY: "Developer ID Application: Example",
    APPLE_API_ISSUER: "issuer",
    APPLE_API_KEY: "key-id",
    APPLE_API_KEY_PATH: "/private/key.p8",
  };
  const windows = {
    ...common,
    WINDOWS_CERTIFICATE_THUMBPRINT: "0123456789ABCDEF0123456789ABCDEF01234567",
    WINDOWS_TIMESTAMP_URL: "https://timestamp.example.test",
  };

  assert.throws(() => validate("token-meter", "universal-apple-darwin", {}, () => true), /TAURI_UPDATER_PUBLIC_KEY/);
  assert.throws(() => validate("other", "universal-apple-darwin", mac, () => true), /Invalid flavor/);
  assert.throws(() => validate("token-meter", "other", mac, () => true), /Invalid target/);
  assert.throws(() => validate("token-meter", "universal-apple-darwin", { ...mac, TAURI_UPDATER_ENDPOINT: "http://example.test" }, () => true), /HTTPS URL/);
  assert.throws(() => validate("token-meter", "universal-apple-darwin", { ...mac, APPLE_API_KEY_PATH: "relative.p8" }, () => true), /notarization credentials/);
  assert.throws(() => validate("token-meter", "universal-apple-darwin", mac, () => false), /notarization credentials/);
  validate("token-meter", "universal-apple-darwin", mac, () => true);
  validate("dev-console", "universal-apple-darwin", {
    ...common,
    APPLE_SIGNING_IDENTITY: "Developer ID Application: Example",
    APPLE_ID: "release@example.test",
    APPLE_PASSWORD: "app-password",
    APPLE_TEAM_ID: "TEAMID",
  });
  validate("runtime-atlas", "x86_64-pc-windows-msvc", windows);

  assert.deepEqual(overlay("universal-apple-darwin", mac), {
    bundle: { createUpdaterArtifacts: true },
    plugins: { updater: { pubkey: "public-key", endpoints: [common.TAURI_UPDATER_ENDPOINT] } },
  });
  assert.deepEqual(overlay("x86_64-pc-windows-msvc", windows), {
    bundle: {
      createUpdaterArtifacts: true,
      windows: {
        digestAlgorithm: "sha256",
        certificateThumbprint: windows.WINDOWS_CERTIFICATE_THUMBPRINT,
        timestampUrl: windows.WINDOWS_TIMESTAMP_URL,
      },
    },
    plugins: { updater: { pubkey: "public-key", endpoints: [common.TAURI_UPDATER_ENDPOINT] } },
  });

  const overlayPath = "/private/release/overlay.json";
  const invocation = command("dev-console", "x86_64-pc-windows-msvc", overlayPath);
  assert.deepEqual(invocation.args, [
    resolve(root, "node_modules/@tauri-apps/cli/tauri.js"),
    "build",
    "--config", resolve(root, "apps/dev-console/tauri.conf.json"),
    "--config", overlayPath,
    "--target", "x86_64-pc-windows-msvc",
    "--bundles", "nsis",
    "--ci",
  ]);
  assert(!invocation.args.includes("--no-sign"));
  assert(!JSON.stringify({ overlay: overlay("x86_64-pc-windows-msvc", windows), invocation, output: successOutput }).includes(common.TAURI_SIGNING_PRIVATE_KEY));
  process.stdout.write(successOutput);
}

function main() {
  if (process.argv.length === 3 && process.argv[2] === "--self-test") return selfTest();
  if (process.argv.length !== 4) {
    throw new Error("Usage: build-release.mjs <token-meter|runtime-atlas|dev-console> <universal-apple-darwin|x86_64-pc-windows-msvc>");
  }

  const [, , flavor, target] = process.argv;
  validate(flavor, target, process.env);
  const directory = mkdtempSync(join(tmpdir(), "dev-console-release-"));
  chmodSync(directory, 0o700);
  const overlayPath = join(directory, "tauri.release.conf.json");
  try {
    writeFileSync(overlayPath, `${JSON.stringify(overlay(target, process.env))}\n`, { mode: 0o600 });
    chmodSync(overlayPath, 0o600);
    const invocation = command(flavor, target, overlayPath);
    const result = spawnSync(invocation.executable, invocation.args, {
      cwd: root,
      env: process.env,
      stdio: "inherit",
    });
    if (result.error) throw new Error("Tauri release build could not be started.");
    if (result.status !== 0) throw new Error(`Tauri release build failed with exit code ${result.status ?? "unknown"}.`);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

main();
