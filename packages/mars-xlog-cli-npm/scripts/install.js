const fs = require("fs");
const https = require("https");
const path = require("path");
const zlib = require("zlib");

const pkg = require("../package.json");

const target = targetTriple(process.platform, process.arch);
const binName = process.platform === "win32" ? "mars-xlog.exe" : "mars-xlog";
const vendorDir = path.join(__dirname, "..", "vendor");
const outPath = path.join(vendorDir, binName);

if (process.env.MARS_XLOG_CLI_SKIP_DOWNLOAD === "1") {
  process.exit(0);
}

if (!target) {
  console.error(`unsupported platform: ${process.platform}/${process.arch}`);
  process.exit(1);
}

const version = process.env.MARS_XLOG_CLI_VERSION || pkg.version;
const baseUrl =
  process.env.MARS_XLOG_CLI_BASE_URL ||
  `https://github.com/peterich-rs/xlog-rs/releases/download/v${version}`;
const asset = `mars-xlog-v${version}-${target}.tar.gz`;
const url = `${baseUrl.replace(/\/$/, "")}/${asset}`;

download(url)
  .then((archive) => {
    const tar = zlib.gunzipSync(archive);
    const binary = extractFromTar(tar, binName);
    fs.mkdirSync(vendorDir, { recursive: true });
    fs.writeFileSync(outPath, binary, { mode: 0o755 });
    fs.chmodSync(outPath, 0o755);
  })
  .catch((error) => {
    console.error(`failed to install mars-xlog from ${url}`);
    console.error(error.message);
    process.exit(1);
  });

function targetTriple(platform, arch) {
  const key = `${platform}:${arch}`;
  const targets = {
    "darwin:x64": "x86_64-apple-darwin",
    "darwin:arm64": "aarch64-apple-darwin",
    "linux:x64": "x86_64-unknown-linux-gnu",
    "win32:x64": "x86_64-pc-windows-msvc"
  };
  return targets[key];
}

function download(url, redirects = 0) {
  return new Promise((resolve, reject) => {
    https
      .get(url, (res) => {
        if (
          res.statusCode >= 300 &&
          res.statusCode < 400 &&
          res.headers.location
        ) {
          if (redirects > 5) {
            reject(new Error("too many redirects"));
            return;
          }
          const next = new URL(res.headers.location, url).toString();
          resolve(download(next, redirects + 1));
          return;
        }

        if (res.statusCode !== 200) {
          reject(new Error(`download returned HTTP ${res.statusCode}`));
          return;
        }

        const chunks = [];
        res.on("data", (chunk) => chunks.push(chunk));
        res.on("end", () => resolve(Buffer.concat(chunks)));
      })
      .on("error", reject);
  });
}

function extractFromTar(tar, expectedName) {
  let offset = 0;
  while (offset + 512 <= tar.length) {
    const header = tar.subarray(offset, offset + 512);
    if (header.every((byte) => byte === 0)) {
      break;
    }

    const name = readString(header, 0, 100);
    const sizeRaw = readString(header, 124, 12).replace(/\0/g, "").trim();
    const size = parseInt(sizeRaw || "0", 8);
    const dataStart = offset + 512;
    const dataEnd = dataStart + size;

    if (path.basename(name) === expectedName) {
      return tar.subarray(dataStart, dataEnd);
    }

    offset = dataStart + Math.ceil(size / 512) * 512;
  }

  throw new Error(`archive did not contain ${expectedName}`);
}

function readString(buffer, start, length) {
  const slice = buffer.subarray(start, start + length);
  const end = slice.indexOf(0);
  return slice.subarray(0, end === -1 ? slice.length : end).toString("utf8");
}
