import { copyFile, cp, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { inflateSync } from "node:zlib";

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

const PNG_SIGNATURE = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
const SMALL_ICON_SIZES = new Set([16, 24, 32, 48, 64]);
const ICON_SIZES = [16, 24, 32, 48, 64, 256];

function readIcoEntries(ico) {
  if (ico.readUInt16LE(0) !== 0 || ico.readUInt16LE(2) !== 1) {
    throw new Error("Expected an ICO file");
  }
  const count = ico.readUInt16LE(4);
  const entries = [];
  for (let index = 0; index < count; index += 1) {
    const offset = 6 + index * 16;
    const width = ico[offset] || 256;
    const height = ico[offset + 1] || 256;
    const size = ico.readUInt32LE(offset + 8);
    const imageOffset = ico.readUInt32LE(offset + 12);
    entries.push({ width, height, payload: ico.subarray(imageOffset, imageOffset + size) });
  }
  return entries;
}

function paeth(a, b, c) {
  const p = a + b - c;
  const pa = Math.abs(p - a);
  const pb = Math.abs(p - b);
  const pc = Math.abs(p - c);
  return pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
}

// Tauri emits non-interlaced, 8-bit RGBA PNGs. Keeping this decoder here avoids
// adding an image package solely to make Windows-compatible ICO DIB entries.
function decodePngRgba(png) {
  if (!png.subarray(0, 8).equals(PNG_SIGNATURE)) throw new Error("Expected a PNG icon entry");
  let position = 8;
  let width;
  let height;
  let bitDepth;
  let colorType;
  const data = [];
  while (position < png.length) {
    const length = png.readUInt32BE(position);
    const type = png.toString("ascii", position + 4, position + 8);
    const chunk = png.subarray(position + 8, position + 8 + length);
    position += length + 12;
    if (type === "IHDR") {
      width = chunk.readUInt32BE(0);
      height = chunk.readUInt32BE(4);
      bitDepth = chunk[8];
      colorType = chunk[9];
      if (bitDepth !== 8 || colorType !== 6 || chunk[12] !== 0) {
        throw new Error("ICO conversion requires a non-interlaced 8-bit RGBA PNG");
      }
    } else if (type === "IDAT") data.push(chunk);
    else if (type === "IEND") break;
  }
  if (!width || !height) throw new Error("PNG entry is missing IHDR");
  const bytesPerPixel = 4;
  const stride = width * bytesPerPixel;
  const filtered = inflateSync(Buffer.concat(data));
  const rgba = Buffer.alloc(stride * height);
  let input = 0;
  for (let y = 0; y < height; y += 1) {
    const filter = filtered[input++];
    const row = y * stride;
    for (let x = 0; x < stride; x += 1) {
      const raw = filtered[input++];
      const left = x >= bytesPerPixel ? rgba[row + x - bytesPerPixel] : 0;
      const up = y > 0 ? rgba[row - stride + x] : 0;
      const upLeft = y > 0 && x >= bytesPerPixel ? rgba[row - stride + x - bytesPerPixel] : 0;
      if (filter === 0) rgba[row + x] = raw;
      else if (filter === 1) rgba[row + x] = (raw + left) & 0xff;
      else if (filter === 2) rgba[row + x] = (raw + up) & 0xff;
      else if (filter === 3) rgba[row + x] = (raw + ((left + up) >> 1)) & 0xff;
      else if (filter === 4) rgba[row + x] = (raw + paeth(left, up, upLeft)) & 0xff;
      else throw new Error(`Unsupported PNG filter: ${filter}`);
    }
  }
  return { width, height, rgba };
}

function pngToIcoBitmap(png) {
  const { width, height, rgba } = decodePngRgba(png);
  const xorStride = width * 4;
  const andStride = Math.ceil(width / 32) * 4;
  const bitmap = Buffer.alloc(40 + xorStride * height + andStride * height);
  bitmap.writeUInt32LE(40, 0); // BITMAPINFOHEADER
  bitmap.writeInt32LE(width, 4);
  bitmap.writeInt32LE(height * 2, 8); // XOR and AND bitmap heights
  bitmap.writeUInt16LE(1, 12);
  bitmap.writeUInt16LE(32, 14);
  bitmap.writeUInt32LE(xorStride * height, 20);
  for (let y = 0; y < height; y += 1) {
    const sourceY = height - 1 - y;
    const xorOffset = 40 + y * xorStride;
    const andOffset = 40 + xorStride * height + y * andStride;
    for (let x = 0; x < width; x += 1) {
      const source = (sourceY * width + x) * 4;
      const target = xorOffset + x * 4;
      bitmap[target] = rgba[source + 2];
      bitmap[target + 1] = rgba[source + 1];
      bitmap[target + 2] = rgba[source];
      bitmap[target + 3] = rgba[source + 3];
      if (rgba[source + 3] === 0) bitmap[andOffset + (x >> 3)] |= 0x80 >> (x & 7);
    }
  }
  return bitmap;
}

function makeWindowsIco(entries) {
  const headerSize = 6 + entries.length * 16;
  const header = Buffer.alloc(headerSize);
  header.writeUInt16LE(1, 2);
  header.writeUInt16LE(entries.length, 4);
  let offset = headerSize;
  for (const [index, entry] of entries.entries()) {
    const directory = 6 + index * 16;
    header[directory] = entry.width === 256 ? 0 : entry.width;
    header[directory + 1] = entry.height === 256 ? 0 : entry.height;
    header.writeUInt16LE(1, directory + 4);
    header.writeUInt16LE(32, directory + 6);
    header.writeUInt32LE(entry.payload.length, directory + 8);
    header.writeUInt32LE(offset, directory + 12);
    offset += entry.payload.length;
  }
  return Buffer.concat([header, ...entries.map((entry) => entry.payload)]);
}

function assertWindowsIcoFormats(ico) {
  const entries = readIcoEntries(ico);
  if (entries.length !== ICON_SIZES.length) throw new Error(`Expected ${ICON_SIZES.length} ICO entries`);
  for (const size of ICON_SIZES) {
    const entry = entries.find((candidate) => candidate.width === size && candidate.height === size);
    if (!entry) throw new Error(`Missing ${size}x${size} ICO entry`);
    const isPng = entry.payload.subarray(0, 8).equals(PNG_SIGNATURE);
    const isBitmap = entry.payload.readUInt32LE(0) === 40;
    if (size === 256 ? !isPng : !isBitmap) {
      throw new Error(`${size}x${size} ICO entry has the wrong payload format`);
    }
  }
  console.log("ICO format check: 16/24/32/48/64 BMP, 256 PNG");
}

async function rewriteWindowsIco() {
  const iconPath = join(icons, "icon.ico");
  const generatedEntries = readIcoEntries(await readFile(iconPath));
  const entries = ICON_SIZES.map((size) => {
    const entry = generatedEntries.find((candidate) => candidate.width === size && candidate.height === size);
    if (!entry) throw new Error(`Tauri did not generate a ${size}x${size} ICO entry`);
    return { ...entry, payload: SMALL_ICON_SIZES.has(size) ? pngToIcoBitmap(entry.payload) : entry.payload };
  });
  const ico = makeWindowsIco(entries);
  assertWindowsIcoFormats(ico);
  await writeFile(iconPath, ico);
}

const temporary = await mkdtemp(join(tmpdir(), "codex-usage-limiter-icons-"));
try {
  generate(join(icons, "app-icon.svg"), icons);
  await rewriteWindowsIco();
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
