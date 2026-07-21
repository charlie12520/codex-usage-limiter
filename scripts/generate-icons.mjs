import { copyFile, cp, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { spawnSync } from "node:child_process";

const root = resolve(import.meta.dirname, "..");
const icons = join(root, "src-tauri", "icons");
const appleAssets = join(root, "src-tauri", "gen", "apple", "Assets.xcassets");
const tauri = process.platform === "win32" ? "npx.cmd" : "npx";

function generate(source, output) {
  const result = spawnSync(tauri, ["tauri", "icon", source, "--output", output], {
    cwd: root,
    stdio: "inherit",
    shell: process.platform === "win32",
  });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status ?? 1);
}

const temporary = await mkdtemp(join(tmpdir(), "codex-usage-limiter-icons-"));
try {
  generate(join(icons, "app-icon.svg"), icons);
  const appAppleAssets = join(temporary, "Assets.xcassets");
  await cp(appleAssets, appAppleAssets, { recursive: true });
  for (const name of ["light", "dark", "gold"]) {
    const output = join(temporary, name);
    generate(join(icons, `tray-icon-${name}.svg`), output);
    await copyFile(join(output, "32x32.png"), join(icons, `tray-icon-${name}.png`));
  }
  await rm(appleAssets, { recursive: true, force: true });
  await cp(appAppleAssets, appleAssets, { recursive: true });
  await copyFile(join(icons, "tray-icon-light.png"), join(icons, "tray-icon.png"));
} finally {
  await rm(temporary, { recursive: true, force: true });
}
