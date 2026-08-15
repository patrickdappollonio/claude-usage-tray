#!/usr/bin/env node

import fs from "node:fs/promises";
import path from "node:path";
import os from "node:os";
import { fileURLToPath } from "node:url";
import { execFile } from "node:child_process";
import { promisify } from "node:util";

const execFileAsync = promisify(execFile);

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const repoRoot = path.join(__dirname, "..", "..");

const releaseVersion = process.env.RELEASE_VERSION;
if (!releaseVersion) {
  console.error("RELEASE_VERSION is required");
  process.exit(1);
}

const distDir = path.join(repoRoot, "dist");
const artifactsDir = process.env.ARTIFACTS_DIR ?? distDir;
const npmDistDir = path.join(distDir, "npm");
const stagingDir = path.join(npmDistDir, "package");
const tarballName = `claude-usage-tray-npm-${releaseVersion}.tgz`;
const tarballPath = path.join(npmDistDir, tarballName);

const binaryName = "claude-usage-tray";

// Archive names must match the tarballs produced by the release build matrix:
// claude-usage-tray_<version>_<linux|darwin>_<amd64|arm64>.tar.gz
const TARGETS = [
  { key: "darwin-arm64", archive: `claude-usage-tray_${releaseVersion}_darwin_arm64.tar.gz` },
  { key: "darwin-x64", archive: `claude-usage-tray_${releaseVersion}_darwin_amd64.tar.gz` },
  { key: "linux-arm64", archive: `claude-usage-tray_${releaseVersion}_linux_arm64.tar.gz` },
  { key: "linux-x64", archive: `claude-usage-tray_${releaseVersion}_linux_amd64.tar.gz` }
];

async function main() {
  await fs.rm(stagingDir, { recursive: true, force: true });
  await fs.mkdir(stagingDir, { recursive: true });
  await fs.mkdir(npmDistDir, { recursive: true });
  await cleanOldTarballs();

  await writePackageJson();
  await copyStaticAssets();

  for (const target of TARGETS) {
    const archivePath = path.join(artifactsDir, target.archive);
    await stageBinary(target, archivePath);
  }

  await packTarball();
  console.log(`Created ${tarballPath}`);
}

async function writePackageJson() {
  const pkgPath = path.join(repoRoot, "npm", "package.json");
  const pkgRaw = await fs.readFile(pkgPath, "utf8");
  const pkg = JSON.parse(pkgRaw);
  pkg.version = releaseVersion;
  const destPath = path.join(stagingDir, "package.json");
  await fs.writeFile(destPath, `${JSON.stringify(pkg, null, 2)}\n`, "utf8");
}

async function copyStaticAssets() {
  await fs.cp(path.join(repoRoot, "npm", "bin"), path.join(stagingDir, "bin"), {
    recursive: true
  });
  await fs.chmod(path.join(stagingDir, "bin", `${binaryName}.js`), 0o755);
  await fs.copyFile(path.join(repoRoot, "npm", "README.md"), path.join(stagingDir, "README.md"));
  await fs.copyFile(path.join(repoRoot, "LICENSE"), path.join(stagingDir, "LICENSE"));
}

async function stageBinary(target, archivePath) {
  try {
    await fs.access(archivePath);
  } catch {
    throw new Error(`Required archive ${archivePath} not found`);
  }

  const tmpDir = await fs.mkdtemp(path.join(os.tmpdir(), "claude-usage-tray-npm-"));
  try {
    await execFileAsync("tar", ["-xzf", archivePath, "-C", tmpDir]);
    const binarySrc = await findBinary(tmpDir, binaryName);
    if (!binarySrc) {
      throw new Error(`Unable to find ${binaryName} inside ${archivePath}`);
    }

    const destDir = path.join(stagingDir, "vendor", target.key);
    await fs.mkdir(destDir, { recursive: true });
    const destPath = path.join(destDir, binaryName);
    await fs.copyFile(binarySrc, destPath);
    await fs.chmod(destPath, 0o755);
    console.log(`Staged binary for ${target.key}`);
  } finally {
    await fs.rm(tmpDir, { recursive: true, force: true });
  }
}

async function findBinary(rootDir, name) {
  const stack = [rootDir];
  while (stack.length > 0) {
    const dir = stack.pop();
    const entries = await fs.readdir(dir, { withFileTypes: true });
    for (const entry of entries) {
      const fullPath = path.join(dir, entry.name);
      if (entry.isDirectory()) {
        stack.push(fullPath);
      } else if (entry.isFile() && entry.name === name) {
        return fullPath;
      }
    }
  }
  return null;
}

async function packTarball() {
  const { stdout, stderr } = await execFileAsync(
    "npm",
    ["pack", "--ignore-scripts", "--json", "--pack-destination", npmDistDir],
    { cwd: stagingDir }
  );
  const filename = await resolvePackedFilename(stdout || stderr);
  if (!filename) {
    throw new Error("npm pack did not return a filename");
  }
  const packedPath = path.join(npmDistDir, filename);
  await fs.rename(packedPath, tarballPath);
}

async function cleanOldTarballs() {
  const entries = await fs.readdir(npmDistDir, { withFileTypes: true });
  await Promise.all(
    entries
      .filter((entry) => entry.isFile() && entry.name.endsWith(".tgz"))
      .map((entry) => fs.rm(path.join(npmDistDir, entry.name), { force: true }))
  );
}

async function resolvePackedFilename(output) {
  const trimmed = output.trim();
  if (trimmed) {
    const jsonStart = trimmed.indexOf("[");
    if (jsonStart === -1) {
      throw new Error(`npm pack output did not contain JSON: ${trimmed}`);
    }

    const packInfo = JSON.parse(trimmed.slice(jsonStart));
    const filename = packInfo.at(-1)?.filename;
    if (filename) {
      return filename;
    }
  }

  const entries = await fs.readdir(npmDistDir, { withFileTypes: true });
  const tarballs = entries
    .filter((entry) => entry.isFile() && entry.name.endsWith(".tgz"))
    .map((entry) => entry.name);

  if (tarballs.length !== 1) {
    throw new Error(
      `npm pack did not identify exactly one generated tarball: ${tarballs.join(", ")}`
    );
  }

  return tarballs[0];
}

await main();
