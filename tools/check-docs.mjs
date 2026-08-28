import fs from "node:fs";
import path from "node:path";

const root = process.cwd();
const skippedDirectories = new Set([".git", "target"]);

function markdownFiles(directory) {
  const files = [];
  for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
    if (entry.isDirectory() && skippedDirectories.has(entry.name)) continue;
    const absolute = path.join(directory, entry.name);
    if (entry.isDirectory()) files.push(...markdownFiles(absolute));
    else if (entry.isFile() && entry.name.endsWith(".md")) files.push(absolute);
  }
  return files;
}

function headingSlug(heading) {
  return heading
    .toLowerCase()
    .replace(/<[^>]+>/g, "")
    .replace(/[`*_~]/g, "")
    .replace(/[^\p{L}\p{N}\s-]/gu, "")
    .trim()
    .replace(/\s+/g, "-")
    .replace(/-+/g, "-");
}

function anchorsFor(contents) {
  const occurrences = new Map();
  const anchors = new Set();
  for (const line of contents.split("\n")) {
    const heading = line.match(/^#{1,6}\s+(.+?)\s*#*$/)?.[1];
    if (!heading) continue;
    const base = headingSlug(heading);
    const occurrence = occurrences.get(base) ?? 0;
    anchors.add(occurrence === 0 ? base : `${base}-${occurrence}`);
    occurrences.set(base, occurrence + 1);
  }
  return anchors;
}

function linkTargets(contents) {
  const targets = [];
  const patterns = [
    /!?\[[^\]]*\]\(([^)]+)\)/g,
    /<(?:a|img)\b[^>]*(?:href|src)=["']([^"']+)["']/gi,
  ];
  for (const pattern of patterns) {
    let match;
    while ((match = pattern.exec(contents)) !== null) targets.push(match[1]);
  }
  return targets;
}

const documents = markdownFiles(root).sort();
const contents = new Map(
  documents.map((file) => [file, fs.readFileSync(file, "utf8")]),
);
const anchors = new Map(
  [...contents].map(([file, text]) => [file, anchorsFor(text)]),
);
const failures = [];
let checkedLinks = 0;

for (const [file, text] of contents) {
  const fenceCount = text
    .split("\n")
    .filter((line) => /^\s*(```|~~~)/.test(line)).length;
  if (fenceCount % 2 !== 0) failures.push(`${file}: unclosed code fence`);

  for (let target of linkTargets(text)) {
    checkedLinks += 1;
    target = target.trim().replace(/^<|>$/g, "").split(/\s+["']/)[0];
    if (/^(?:https?:\/\/|mailto:|data:)/.test(target)) continue;

    const [rawFile, rawFragment] = target.split("#");
    let decodedFile;
    let decodedFragment;
    try {
      decodedFile = decodeURIComponent(rawFile || path.basename(file));
      decodedFragment = rawFragment && decodeURIComponent(rawFragment).toLowerCase();
    } catch {
      failures.push(`${file}: invalid URL encoding in ${target}`);
      continue;
    }

    const resolved = path.resolve(path.dirname(file), decodedFile);
    if (!fs.existsSync(resolved)) {
      failures.push(`${file}: missing relative target ${target}`);
      continue;
    }
    if (
      decodedFragment &&
      resolved.endsWith(".md") &&
      !anchors.get(resolved)?.has(decodedFragment)
    ) {
      failures.push(`${file}: missing heading anchor ${target}`);
    }
  }
}

const manifest = fs.readFileSync(path.join(root, "Cargo.toml"), "utf8");
const packageVersion = manifest.match(/^version = "([^"]+)"/m)?.[1];
const readme = contents.get(path.join(root, "README.md"));
if (!packageVersion) failures.push("Cargo.toml: package version is missing");
else {
  if (!readme?.includes(`version=${packageVersion}`)) {
    failures.push(`README.md: Debian example is not version ${packageVersion}`);
  }
  if (!readme?.includes(`tag=v${packageVersion}`)) {
    failures.push(`README.md: archive example is not tag v${packageVersion}`);
  }
}

if (failures.length > 0) {
  for (const failure of failures) console.error(`error: ${failure}`);
  process.exit(1);
}

console.log(
  `ok: ${documents.length} Markdown files, ${checkedLinks} links, and release examples for ${packageVersion}`,
);
