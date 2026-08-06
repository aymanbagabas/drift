// Postinstall: download the prebuilt drift binary for this platform from the
// matching GitHub release and unpack it into vendor/. The archive names mirror
// the GoReleaser `archives.name_template` (drift_<version>_<os>_<arch>.<ext>).
const fs = require("fs");
const os = require("os");
const path = require("path");
const { execFileSync } = require("child_process");

const OWNER = "aymanbagabas";
const REPO = "drift";
const { version } = require("./package.json");

const PLATFORM = { darwin: "darwin", linux: "linux", win32: "windows" }[process.platform];
const ARCH = { x64: "amd64", arm64: "arm64" }[process.arch];

if (!PLATFORM || !ARCH) {
  console.error(`drift: unsupported platform ${process.platform}/${process.arch}`);
  process.exit(1);
}

const ext = PLATFORM === "windows" ? "zip" : "tar.gz";
const asset = `${REPO}_${version}_${PLATFORM}_${ARCH}.${ext}`;
const url = `https://github.com/${OWNER}/${REPO}/releases/download/v${version}/${asset}`;
const binName = PLATFORM === "windows" ? "drift.exe" : "drift";
const vendor = path.join(__dirname, "vendor");

async function main() {
  fs.mkdirSync(vendor, { recursive: true });
  const archivePath = path.join(os.tmpdir(), asset);

  const res = await fetch(url, { redirect: "follow" });
  if (!res.ok) {
    throw new Error(`download failed (${res.status}) for ${url}`);
  }
  fs.writeFileSync(archivePath, Buffer.from(await res.arrayBuffer()));

  // bsdtar (macOS, Linux, Windows 10+) extracts both .tar.gz and .zip.
  execFileSync("tar", ["-xf", archivePath, "-C", vendor, binName], { stdio: "inherit" });
  fs.unlinkSync(archivePath);

  const binPath = path.join(vendor, binName);
  if (!fs.existsSync(binPath)) {
    throw new Error(`binary ${binName} not found in ${asset}`);
  }
  if (PLATFORM !== "windows") fs.chmodSync(binPath, 0o755);
}

main().catch((err) => {
  console.error(`drift: failed to install binary: ${err.message}`);
  process.exit(1);
});
