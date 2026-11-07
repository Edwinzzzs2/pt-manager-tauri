import { spawnSync } from "node:child_process";
import { copyFile, mkdir, readFile, readdir, writeFile } from "node:fs/promises";
import path from "node:path";

const artifactsRoot = path.resolve("artifacts");
const outputRoot = path.resolve("release-assets");
const tag = requiredEnv("RELEASE_TAG");
const version = requiredEnv("RELEASE_VERSION").replace(/^v/, "");
const repository = requiredEnv("RELEASE_REPOSITORY");

await mkdir(outputRoot, { recursive: true });

const windows = await prepareWindowsArtifact();
const macIntel = await prepareMacArtifact("darwin-x86_64", "x64");
const macArm = await prepareMacArtifact("darwin-aarch64", "arm64");

// Tauri 按运行机器匹配 OS-ARCH，同一个清单必须同时包含三种发布目标。
const manifest = {
  version,
  notes: releaseNotes(),
  pub_date: new Date().toISOString(),
  platforms: {
    "windows-x86_64": updaterEntry(windows),
    "darwin-x86_64": updaterEntry(macIntel),
    "darwin-aarch64": updaterEntry(macArm),
  },
};

await writeFile(
  path.join(outputRoot, "latest.json"),
  `${JSON.stringify(manifest)}\n`,
  "utf8",
);

function releaseNotes() {
  const tagRef = `refs/tags/${tag}`;
  // 带注释的标签可提供人工摘要，普通标签则使用上次发布以来的提交标题。
  if (gitOutput("cat-file", "-t", tagRef) === "tag") {
    const annotation = gitOutput("for-each-ref", "--format=%(contents:subject)%0a%(contents:body)", tagRef);
    if (annotation) return annotation;
  }

  const previousTag = gitOutput(
    "describe", "--tags", "--abbrev=0", "--match", "v[0-9]*", `${tagRef}^`,
  );
  const subjects = previousTag
    ? gitOutput("log", "--no-merges", "-5", "--format=• %s", `${previousTag}..${tagRef}`)
    : gitOutput("log", "-1", "--format=• %s", tagRef);
  return subjects || undefined;
}

function gitOutput(...args) {
  const result = spawnSync("git", args, { encoding: "utf8" });
  return result.status === 0 ? result.stdout.trim() : "";
}

async function prepareWindowsArtifact() {
  const root = path.join(artifactsRoot, "windows-x86_64");
  const files = await listFiles(root);
  const installer = pickFile(files, (file) => file.endsWith(".exe"), "Windows 安装包");
  const signature = await readSignature(files, installer);
  const fileName = `PT-Manager-${tag}-windows-x64-setup.exe`;

  await copyFile(installer, path.join(outputRoot, fileName));
  return { fileName, signature };
}

async function prepareMacArtifact(artifactName, architectureName) {
  const root = path.join(artifactsRoot, artifactName);
  const files = await listFiles(root);
  const updaterArchive = pickFile(
    files,
    (file) => file.endsWith(".app.tar.gz"),
    `${architectureName} macOS 更新包`,
  );
  const dmg = pickFile(files, (file) => file.endsWith(".dmg"), `${architectureName} macOS DMG`);
  const signature = await readSignature(files, updaterArchive);
  const updaterFileName = `PT-Manager-${tag}-macos-${architectureName}.app.tar.gz`;
  const dmgFileName = `PT-Manager-${tag}-macos-${architectureName}.dmg`;

  await copyFile(updaterArchive, path.join(outputRoot, updaterFileName));
  await copyFile(dmg, path.join(outputRoot, dmgFileName));
  return { fileName: updaterFileName, signature };
}

function updaterEntry({ fileName, signature }) {
  return {
    signature,
    url: `https://github.com/${repository}/releases/download/${tag}/${fileName}`,
  };
}

async function readSignature(files, signedFile) {
  const exactSignature = `${signedFile}.sig`;
  const signatureFile = files.includes(exactSignature)
    ? exactSignature
    : pickFile(files, (file) => file.endsWith(".sig"), `${path.basename(signedFile)} 签名`);
  return (await readFile(signatureFile, "utf8")).trim();
}

async function listFiles(root) {
  const files = [];
  const entries = await readdir(root, { withFileTypes: true });
  for (const entry of entries) {
    const entryPath = path.join(root, entry.name);
    if (entry.isDirectory()) {
      files.push(...(await listFiles(entryPath)));
    } else if (entry.isFile()) {
      files.push(entryPath);
    }
  }
  return files;
}

function pickFile(files, predicate, description) {
  const file = files.find(predicate);
  if (!file) {
    const foundFiles = files.map((item) => path.basename(item)).join(", ") || "无";
    throw new Error(`未找到${description}；当前产物：${foundFiles}`);
  }
  return file;
}

function requiredEnv(name) {
  const value = process.env[name]?.trim();
  if (!value) {
    throw new Error(`缺少环境变量 ${name}`);
  }
  return value;
}
