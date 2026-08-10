import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import { createReadStream } from "node:fs";
import {
  chmod,
  mkdir,
  mkdtemp,
  readFile,
  realpath,
  rename,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { basename, dirname, join, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const prefix = "tokenmeter-updater-qa-";
const appName = "TokenMeter Updater QA";
const binaryName = "TokenMeterUpdaterQA";
const productionIdentifier = "local.tokenmeter.app";
const qaIdentifierPrefix = "local.tokenmeter.updaterqa.";
const preflightArgument = "--token-meter-updater-qa-preflight";
const command = process.argv[2] || "start";

function assertQaIsolation(directory, identifier, dataDirectory) {
  assert.match(identifier, /^local\.tokenmeter\.updaterqa\.[0-9a-f]{24}$/);
  assert.notEqual(identifier, productionIdentifier);
  assert.equal(resolve(dataDirectory), join(resolve(directory), "data", identifier));
}

function preflightOutput(output, identifier, dataDirectory) {
  assert.deepEqual(output.split("\n"), [identifier, dataDirectory]);
}

async function qaDirectory(value) {
  const directory = await realpath(resolve(value));
  const temporaryRoot = await realpath(tmpdir());
  if (dirname(directory) !== temporaryRoot || !basename(directory).startsWith(prefix)) {
    throw new Error("Refusing a QA directory outside the system temporary directory");
  }
  return directory;
}

async function atomicWrite(path, contents, mode = 0o600) {
  const temporary = `${path}.${randomBytes(8).toString("hex")}`;
  await writeFile(temporary, contents, { flag: "wx", mode });
  await rename(temporary, path);
}

async function setMode(directory, mode) {
  if (mode !== "valid" && mode !== "tampered") throw new Error("Mode must be valid or tampered");
  directory = await qaDirectory(directory);
  await stat(join(directory, "mode"));
  await atomicWrite(join(directory, "mode"), `${mode}\n`);
  console.log(`Updater QA mode: ${mode}`);
}

async function stop(directory) {
  directory = await qaDirectory(directory);
  const control = JSON.parse(await readFile(join(directory, "control.json"), "utf8"));
  const response = await fetch(control.stopUrl, { method: "POST" });
  if (!response.ok) throw new Error(`Stop failed with HTTP ${response.status}`);
  console.log("Updater QA server stopped; temporary files are being removed.");
}

async function launch(directory) {
  directory = await qaDirectory(directory);
  const control = JSON.parse(await readFile(join(directory, "control.json"), "utf8"));
  assertQaIsolation(directory, control.bundleIdentifier, control.dataDirectory);
  const response = await fetch(control.launchUrl, { method: "POST" });
  if (!response.ok) throw new Error(`Launch failed with HTTP ${response.status}`);
  const launched = await response.json();
  assert.equal(launched.bundleIdentifier, control.bundleIdentifier);
  assert.ok(Number.isSafeInteger(launched.pid) && launched.pid > 0);
  console.log(`${appName} launched with isolated data.`);
  console.log(`Safe accessibility attach: existing PID ${launched.pid}`);
  console.log(`If the tool requires an app selector, use exact bundle identifier ${control.bundleIdentifier}.`);
}

function run(program, args, options = {}) {
  return new Promise((accept, reject) => {
    const child = spawn(program, args, {
      cwd: root,
      env: options.env,
      stdio: options.quiet ? ["ignore", "ignore", "inherit"] : "inherit",
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
    const child = spawn(program, args, { cwd: root, env, stdio: ["ignore", "pipe", "inherit"] });
    let output = "";
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk) => (output += chunk));
    child.once("error", reject);
    child.once("exit", (status) =>
      status === 0 ? accept(output.trim()) : reject(new Error(`${program} failed (${status})`)),
    );
  });
}

async function sha256(path) {
  const hash = createHash("sha256");
  for await (const chunk of createReadStream(path)) hash.update(chunk);
  return hash.digest("hex");
}

function tamper(signature) {
  const encoded = signature.trim();
  const lines = Buffer.from(encoded, "base64").toString("utf8").split("\n");
  if (lines.length < 4 || !/^[A-Za-z0-9+/]+={0,2}$/.test(lines[1])) {
    throw new Error("Unexpected updater signature format");
  }
  const index = Math.floor(lines[1].length / 2);
  lines[1] = `${lines[1].slice(0, index)}${lines[1][index] === "A" ? "B" : "A"}${lines[1].slice(index + 1)}`;
  const tampered = Buffer.from(lines.join("\n")).toString("base64");
  assert.notEqual(tampered, encoded);
  assert.equal(Buffer.from(tampered, "base64").toString("utf8").split("\n")[1].length, lines[1].length);
  return tampered;
}

async function start() {
  if (process.platform !== "darwin") throw new Error("This QA harness requires macOS");
  if (process.argv.length > 3) throw new Error("start does not accept arguments");

  const directory = await mkdtemp(join(await realpath(tmpdir()), prefix));
  await chmod(directory, 0o700);
  const home = join(directory, "home");
  const keys = join(directory, "keys");
  const identifier = `${qaIdentifierPrefix}${randomBytes(12).toString("hex")}`;
  const dataDirectory = join(directory, "data", identifier);
  assertQaIsolation(directory, identifier, dataDirectory);
  await mkdir(home, { recursive: true, mode: 0o700 });
  await mkdir(keys, { mode: 0o700 });
  await mkdir(dirname(dataDirectory), { mode: 0o700 });

  let activeChild;
  let appGroupPid;
  let server;
  let cleaning = false;
  const appGroupAlive = () => {
    if (!appGroupPid) return false;
    try {
      process.kill(-appGroupPid, 0);
      return true;
    } catch (error) {
      if (error.code === "ESRCH") return false;
      throw error;
    }
  };
  const stopApp = async () => {
    if (!appGroupAlive()) return;
    process.kill(-appGroupPid, "SIGTERM");
    for (let attempt = 0; attempt < 50 && appGroupAlive(); attempt += 1) await delay(100);
    if (appGroupAlive()) process.kill(-appGroupPid, "SIGKILL");
    while (appGroupAlive()) await delay(50);
  };
  const cleanup = async () => {
    if (cleaning) return;
    cleaning = true;
    activeChild?.kill("SIGTERM");
    await stopApp();
    if (server?.listening) await new Promise((done) => server.close(done));
    await rm(directory, { recursive: true, force: true });
  };
  const interrupted = () => void cleanup().finally(() => process.exit(130));
  process.once("SIGINT", interrupted);
  process.once("SIGTERM", interrupted);

  try {
    const stopToken = randomBytes(32).toString("hex");
    const modePath = join(directory, "mode");
    const validManifestPath = join(directory, "latest-valid.json");
    const tamperedManifestPath = join(directory, "latest-tampered.json");
    const baseApp = join(directory, "targets", "base", "release", "bundle", "macos", `${appName}.app`);
    const binary = join(baseApp, "Contents", "MacOS", binaryName);
    const updateArtifact = join(directory, "targets", "update", "release", "bundle", "macos", `${appName}.app.tar.gz`);

    server = createServer(async (request, response) => {
      try {
        const url = new URL(request.url, "http://127.0.0.1");
        if (request.method === "POST" && url.pathname === `/stop/${stopToken}`) {
          response.writeHead(204).end();
          setImmediate(() => void cleanup());
          return;
        }
        if (request.method === "POST" && url.pathname === `/launch/${stopToken}`) {
          if (appGroupAlive()) return response.writeHead(409).end();
          const appEnv = { ...process.env, HOME: home };
          delete appEnv.CODEX_HOME;
          delete appEnv.TOKEN_METER_UPDATER_QA_ROOT;
          const child = spawn(binary, [], { detached: true, env: appEnv, stdio: "ignore" });
          await new Promise((accept, reject) => {
            child.once("spawn", accept);
            child.once("error", reject);
          });
          appGroupPid = child.pid;
          response.writeHead(200, { "content-type": "application/json", "cache-control": "no-store" });
          response.end(JSON.stringify({ pid: child.pid, bundleIdentifier: identifier }));
          return;
        }
        if (request.method !== "GET") return response.writeHead(405).end();
        if (url.pathname === "/latest.json") {
          const mode = (await readFile(modePath, "utf8")).trim();
          const manifest = mode === "valid" ? validManifestPath : tamperedManifestPath;
          response.writeHead(200, { "content-type": "application/json", "cache-control": "no-store" });
          return createReadStream(manifest).pipe(response);
        }
        if (url.pathname === "/update.app.tar.gz") {
          response.writeHead(200, { "content-type": "application/gzip", "cache-control": "no-store" });
          return createReadStream(updateArtifact).pipe(response);
        }
        response.writeHead(url.pathname === "/health" ? 204 : 404).end();
      } catch {
        if (!response.headersSent) response.writeHead(500);
        response.end();
      }
    });
    await new Promise((accept, reject) => {
      server.once("error", reject);
      server.listen(0, "127.0.0.1", accept);
    });
    const port = server.address().port;
    const manifestUrl = `http://127.0.0.1:${port}/latest.json`;

    const rustupCargo = await capture("rustup", ["which", "cargo"]);
    const env = { ...process.env, PATH: `${dirname(rustupCargo)}:${process.env.PATH || ""}` };
    const privateKey = join(keys, "updater.key");
    const publicKey = `${privateKey}.pub`;
    await run("npx", ["tauri", "signer", "generate", "--ci", "--write-keys", privateKey], {
      env,
      quiet: true,
      onChild: (child) => (activeChild = child),
    });
    await chmod(privateKey, 0o600);
    await chmod(publicKey, 0o644);
    const publicKeyContent = (await readFile(publicKey, "utf8")).trim();
    if (!publicKeyContent) throw new Error("Updater public key is empty");

    const signingEnv = { ...env, TAURI_SIGNING_PRIVATE_KEY: privateKey };
    for (const [name, version] of [
      ["base", "0.1.0"],
      ["update", "0.1.1"],
    ]) {
      const target = join(directory, "targets", name);
      const overlay = join(directory, `${name}.json`);
      await atomicWrite(
        overlay,
        `${JSON.stringify(
          {
            productName: appName,
            mainBinaryName: binaryName,
            identifier,
            version,
            bundle: { createUpdaterArtifacts: true, macOS: { signingIdentity: "-" } },
            plugins: {
              updater: {
                pubkey: publicKeyContent,
                endpoints: [manifestUrl],
                dangerousInsecureTransportProtocol: true,
              },
            },
          },
          null,
          2,
        )}\n`,
      );
      await run(
        "npx",
        ["tauri", "build", "--ci", "--bundles", "app", "--config", "apps/token-meter/tauri.conf.json", "--config", overlay],
        {
          env: {
            ...signingEnv,
            CARGO_TARGET_DIR: target,
            TOKEN_METER_UPDATER_QA_ROOT: dataDirectory,
          },
          onChild: (child) => (activeChild = child),
        },
      );
    }

    const updateApp = join(directory, "targets", "update", "release", "bundle", "macos", `${appName}.app`);
    const signaturePath = `${updateArtifact}.sig`;
    for (const path of [baseApp, updateApp, updateArtifact, signaturePath]) await stat(path);
    await run("/usr/bin/codesign", ["--verify", "--deep", "--strict", baseApp], { env });
    await run("/usr/bin/codesign", ["--verify", "--deep", "--strict", updateApp], { env });

    for (const [app, version] of [
      [baseApp, "0.1.0"],
      [updateApp, "0.1.1"],
    ]) {
      const plist = join(app, "Contents", "Info.plist");
      const plistIdentifier = await capture("/usr/libexec/PlistBuddy", ["-c", "Print :CFBundleIdentifier", plist], env);
      const displayName = await capture("/usr/libexec/PlistBuddy", ["-c", "Print :CFBundleDisplayName", plist], env);
      const executable = await capture("/usr/libexec/PlistBuddy", ["-c", "Print :CFBundleExecutable", plist], env);
      const foundVersion = await capture("/usr/libexec/PlistBuddy", ["-c", "Print :CFBundleShortVersionString", plist], env);
      if (plistIdentifier !== identifier || displayName !== appName || executable !== binaryName || foundVersion !== version) {
        throw new Error(`Unexpected bundle contract: ${plistIdentifier} ${displayName} ${executable} ${foundVersion}`);
      }
    }

    const runtimeEnv = { ...env };
    delete runtimeEnv.TOKEN_METER_UPDATER_QA_ROOT;
    for (const preflightBinary of [binary, join(updateApp, "Contents", "MacOS", binaryName)]) {
      preflightOutput(
        await capture(preflightBinary, [preflightArgument], runtimeEnv),
        identifier,
        dataDirectory,
      );
    }
    await assert.rejects(stat(dataDirectory), { code: "ENOENT" });

    const signature = (await readFile(signaturePath, "utf8")).trim();
    const platform = process.arch === "arm64" ? "darwin-aarch64" : "darwin-x86_64";
    const manifest = (value) => ({
      version: "0.1.1",
      notes: "Ephemeral local updater QA",
      pub_date: new Date().toISOString(),
      platforms: {
        [platform]: { signature: value, url: `http://127.0.0.1:${port}/update.app.tar.gz` },
      },
    });
    await atomicWrite(validManifestPath, `${JSON.stringify(manifest(signature), null, 2)}\n`);
    await atomicWrite(tamperedManifestPath, `${JSON.stringify(manifest(tamper(signature)), null, 2)}\n`);
    await writeFile(modePath, "tampered\n", { flag: "wx", mode: 0o600 });
    await atomicWrite(
      join(directory, "control.json"),
      `${JSON.stringify({
        bundleIdentifier: identifier,
        dataDirectory,
        launchUrl: `http://127.0.0.1:${port}/launch/${stopToken}`,
        stopUrl: `http://127.0.0.1:${port}/stop/${stopToken}`,
      })}\n`,
    );
    await atomicWrite(
      join(directory, "README.txt"),
      `Launch only with the harness command below.\nAttach accessibility tooling to the existing PID reported by Launch.\nIf a tool requires an app selector, use exact bundle identifier: ${identifier}\nNever ask accessibility tooling to launch an app path or executable.\n`,
    );

    console.log(`Updater QA workspace: ${directory}`);
    console.log(`Initial mode: tampered`);
    console.log(`Base app: ${baseApp}`);
    console.log(`QA bundle identifier: ${identifier}`);
    console.log(`QA local data directory: ${dataDirectory}`);
    console.log("Static bundle and no-UI runtime isolation preflight: passed");
    console.log(`Update artifact SHA-256: ${await sha256(updateArtifact)}`);
    console.log(`Launch: npm run qa:updater:macos -- launch ${JSON.stringify(directory)}`);
    console.log(`Valid mode: npm run qa:updater:macos -- mode ${JSON.stringify(directory)} valid`);
    console.log(`Tampered mode: npm run qa:updater:macos -- mode ${JSON.stringify(directory)} tampered`);
    console.log(`Stop: npm run qa:updater:macos -- stop ${JSON.stringify(directory)}`);
    console.log("The server stays active until Stop or Ctrl-C.");
  } catch (error) {
    await cleanup();
    throw error;
  }
}

if (command === "start") await start();
else if (command === "mode" && process.argv.length === 5) await setMode(process.argv[3], process.argv[4]);
else if (command === "launch" && process.argv.length === 4) await launch(process.argv[3]);
else if (command === "stop" && process.argv.length === 4) await stop(process.argv[3]);
else throw new Error("Usage: prepare-updater-macos-qa.mjs [start | launch <workspace> | mode <workspace> <valid|tampered> | stop <workspace>]");
