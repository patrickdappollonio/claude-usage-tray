#!/usr/bin/env node
// Copies website/ into _site/ and writes the latest release version and the
// all-time download count (GitHub releases plus npm) into the page, so
// visitors' browsers never have to ask either API themselves.
//
// Usage: node scripts/build-website.mjs
// GITHUB_TOKEN is optional; without it the API allows 60 requests an hour.

import { cpSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const REPO = process.env.GITHUB_REPOSITORY || "patrickdappollonio/claude-usage-tray";
const NPM_PACKAGE = "@patrickdappollonio/claude-usage-tray";
const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const source = join(root, "website");
const output = join(root, "_site");

async function fetchReleases() {
  const headers = { Accept: "application/vnd.github+json", "User-Agent": "claude-usage-tray-website" };
  if (process.env.GITHUB_TOKEN) headers.Authorization = `Bearer ${process.env.GITHUB_TOKEN}`;

  const releases = [];
  for (let page = 1; ; page++) {
    const res = await fetch(`https://api.github.com/repos/${REPO}/releases?per_page=100&page=${page}`, { headers });
    if (!res.ok) throw new Error(`GitHub API answered ${res.status} for releases page ${page}`);
    const list = await res.json();
    releases.push(...list);
    if (list.length < 100) return releases;
  }
}

// npm answers at most 18 months per request, so walk from the package's
// creation date in chunks. The package bundles its own binaries and never
// downloads from GitHub, so the two counts don't overlap.
async function fetchNpmDownloads() {
  const meta = await fetch(`https://registry.npmjs.org/${NPM_PACKAGE.replace("/", "%2F")}`);
  if (!meta.ok) throw new Error(`npm registry answered ${meta.status}`);
  const created = new Date((await meta.json()).time.created);

  const day = (date) => date.toISOString().slice(0, 10);
  const today = new Date();
  let total = 0;
  for (let from = created; from <= today; ) {
    const to = new Date(Math.min(from.getTime() + 500 * 86400000, today.getTime()));
    const res = await fetch(`https://api.npmjs.org/downloads/point/${day(from)}:${day(to)}/${NPM_PACKAGE}`);
    if (!res.ok) throw new Error(`npm downloads API answered ${res.status}`);
    total += (await res.json()).downloads;
    from = new Date(to.getTime() + 86400000);
  }
  return total;
}

const releases = await fetchReleases();
const npmDownloads = await fetchNpmDownloads();

// The same number as the README's "GitHub Downloads (all assets, all releases)" badge.
const githubDownloads = releases
  .flatMap((release) => release.assets)
  .reduce((total, asset) => total + asset.download_count, 0);

// The list is newest first; drafts and prereleases are not what people should install.
const latest = releases.find((release) => !release.draft && !release.prerelease);
if (!latest) throw new Error("no published release found");
const version = latest.tag_name.replace(/^v/, "");

rmSync(output, { recursive: true, force: true });
cpSync(source, output, { recursive: true });

const page = join(output, "index.html");
const html = readFileSync(page, "utf8");
const marker = /(<script type="application\/json" id="release-data">)[^<]*(<\/script>)/;
if (!marker.test(html)) throw new Error("release-data block not found in website/index.html");
const data = { version, downloads: githubDownloads + npmDownloads, github: githubDownloads, npm: npmDownloads };
writeFileSync(page, html.replace(marker, `$1${JSON.stringify(data)}$2`));

console.log(`Built _site/ with version ${version} and ${data.downloads} downloads (${githubDownloads} GitHub, ${npmDownloads} npm)`);
