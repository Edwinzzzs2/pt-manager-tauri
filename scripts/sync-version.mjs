import { readFile, writeFile } from "node:fs/promises";

const version = requiredEnv("RELEASE_VERSION").replace(/^v/, "");
if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/.test(version)) {
  throw new Error(`Tag 版本格式无效: ${version}`);
}

await updateJson("package.json", (data) => {
  data.version = version;
});

await updateJson("package-lock.json", (data) => {
  data.version = version;
  if (data.packages?.[""]) {
    data.packages[""].version = version;
  }
});

await updateJson("src-tauri/tauri.conf.json", (data) => {
  data.version = version;
});

const cargoPath = "src-tauri/Cargo.toml";
const cargo = await readFile(cargoPath, "utf8");
// 只替换首个 [package] 内的版本，避免误改依赖或工作区配置。
const nextCargo = cargo.replace(
  /(\[package\][\s\S]*?\nversion\s*=\s*)"[^"]+"/,
  `$1"${version}"`,
);
if (nextCargo === cargo) {
  throw new Error("未找到 Cargo package 版本");
}
await writeFile(cargoPath, nextCargo, "utf8");

async function updateJson(file, mutate) {
  const data = JSON.parse(await readFile(file, "utf8"));
  mutate(data);
  await writeFile(file, `${JSON.stringify(data, null, 2)}\n`, "utf8");
}

function requiredEnv(name) {
  const value = process.env[name]?.trim();
  if (!value) {
    throw new Error(`缺少环境变量 ${name}`);
  }
  return value;
}
