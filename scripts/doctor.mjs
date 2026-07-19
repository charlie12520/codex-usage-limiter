import fs from "node:fs";
import path from "node:path";

const strict = process.argv.includes("--strict");

function isExecutableFile(filePath) {
  try {
    const stat = fs.statSync(filePath);
    if (!stat.isFile()) return false;
    if (process.platform === "win32") return true;
    fs.accessSync(filePath, fs.constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

function hasCommand(command) {
  const pathValue = process.env.PATH;
  if (!pathValue) return false;

  const dirs = pathValue.split(path.delimiter).filter(Boolean);

  if (process.platform !== "win32") {
    return dirs.some((dir) => isExecutableFile(path.join(dir, command)));
  }

  const pathExtValue = process.env.PATHEXT ?? ".EXE;.CMD;.BAT;.COM";
  const exts = pathExtValue.split(";").filter(Boolean);
  const hasExtension = path.extname(command) !== "";

  for (const dir of dirs) {
    if (hasExtension) {
      if (isExecutableFile(path.join(dir, command))) return true;
      continue;
    }
    for (const ext of exts) {
      if (isExecutableFile(path.join(dir, `${command}${ext}`))) return true;
    }
  }

  return false;
}

// The whisper/cpal dictation stack (which needed CMake + LLVM) is gone;
// building now only needs Node and a stable Rust toolchain.
const missing = [];
if (!hasCommand("cargo")) missing.push("rust (cargo)");

if (missing.length === 0) {
  console.log("Doctor: OK");
  process.exit(0);
}

console.log(`Doctor: missing dependencies: ${missing.join(" ")}`);
console.log("Install Rust from: https://rustup.rs/");

process.exit(strict ? 1 : 0);
