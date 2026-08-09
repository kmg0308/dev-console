import { copyFileSync, mkdirSync } from "node:fs";
import { chmod } from "node:fs/promises";
import { spawnSync } from "node:child_process";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const triple = process.env.TAURI_ENV_TARGET_TRIPLE;
if (!triple) throw new Error("TAURI_ENV_TARGET_TRIPLE is required");

const release = process.env.TAURI_ENV_DEBUG !== "true";
const target = resolve(root, process.env.CARGO_TARGET_DIR || "target");
const extension = triple.includes("windows") ? ".exe" : "";
const helpers = ["runtime-atlas-supervisor", "runtime-atlas"];

function run(command, args) {
  const result = spawnSync(command, args, { cwd: root, stdio: "inherit" });
  if (result.status !== 0) throw new Error(`${command} failed with status ${result.status}`);
}

function build(helper, targetTriple) {
  const packageName = helper === "runtime-atlas" ? "runtime-atlas-cli" : helper;
  const args = ["build", "-p", packageName, "--bin", helper, "--target", targetTriple];
  if (release) args.push("--release");
  run("cargo", args);
  return join(target, targetTriple, release ? "release" : "debug", `${helper}${targetTriple.includes("windows") ? ".exe" : ""}`);
}

for (const helper of helpers) {
  const destination = join(root, "src-tauri", "binaries", `${helper}-${triple}${extension}`);
  mkdirSync(dirname(destination), { recursive: true });
  if (triple === "universal-apple-darwin") {
    const arm = build(helper, "aarch64-apple-darwin");
    const intel = build(helper, "x86_64-apple-darwin");
    copyFileSync(arm, join(root, "src-tauri", "binaries", `${helper}-aarch64-apple-darwin`));
    copyFileSync(intel, join(root, "src-tauri", "binaries", `${helper}-x86_64-apple-darwin`));
    run("lipo", [arm, intel, "-create", "-output", destination]);
  } else {
    copyFileSync(build(helper, triple), destination);
  }
  if (!extension) {
    await chmod(destination, 0o755);
    if (helper === "runtime-atlas" && triple.endsWith("apple-darwin")) {
      const appHelper = join(root, "src-tauri", "binaries", "runtime-atlas-app-helper");
      copyFileSync(destination, appHelper);
      await chmod(appHelper, 0o755);
    }
    if (triple === "universal-apple-darwin") {
      await chmod(join(root, "src-tauri", "binaries", `${helper}-aarch64-apple-darwin`), 0o755);
      await chmod(join(root, "src-tauri", "binaries", `${helper}-x86_64-apple-darwin`), 0o755);
    }
  }
}
