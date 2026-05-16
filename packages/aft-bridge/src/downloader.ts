/**
 * Auto-download the AFT binary from GitHub releases.
 *
 * Resolution order (in resolver.ts):
 *   1. Cached binary in ~/.cache/aft/bin/
 *   2. npm platform package (@cortexkit/aft-darwin-arm64, etc.)
 *   3. PATH lookup (which aft)
 *   4. ~/.cargo/bin/aft
 *   5. Auto-download from GitHub releases (this module)
 *
 * Cache dir respects XDG_CACHE_HOME on Linux/macOS and LOCALAPPDATA on Windows.
 */

import { chmodSync, existsSync, mkdirSync, unlinkSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { error, log, warn } from "./active-logger.js";
import { PLATFORM_ARCH_MAP, PLATFORM_ASSET_MAP } from "./platform.js";

const REPO = "cortexkit/aft";

/** Get the cache directory, respecting XDG_CACHE_HOME / LOCALAPPDATA. */
export function getCacheDir(): string {
  if (process.platform === "win32") {
    const localAppData = process.env.LOCALAPPDATA || process.env.APPDATA;
    const base = localAppData || join(homedir(), "AppData", "Local");
    return join(base, "aft", "bin");
  }

  const base = process.env.XDG_CACHE_HOME || join(homedir(), ".cache");
  return join(base, "aft", "bin");
}

/** Binary name for the current platform. */
export function getBinaryName(): string {
  return process.platform === "win32" ? "aft.exe" : "aft";
}

/** Return the cached binary path if it exists, otherwise null.
 *  Checks the version-specific cache directory only.
 *  The legacy flat cache (~/.cache/aft/bin/aft) is intentionally NOT checked
 *  because it can be overwritten by other instances, corrupting running processes. */
export function getCachedBinaryPath(version?: string): string | null {
  if (!version) return null;
  const binaryPath = join(getCacheDir(), version, getBinaryName());
  return existsSync(binaryPath) ? binaryPath : null;
}

/**
 * Download the AFT binary for the current platform from GitHub releases.
 *
 * @param version - Git tag to download from. Accepts either `"v0.1.0"` or
 *   `"0.1.0"`; the `v` prefix is normalized internally. If omitted, fetches
 *   the latest release tag via the GitHub API.
 * @returns Absolute path to the downloaded binary, or null on failure.
 */
export async function downloadBinary(version?: string): Promise<string | null> {
  // Resolve via the shared platform table rather than concatenating
  // process.platform + process.arch directly. This matters for Windows
  // ARM64, which the table maps to win32-x64 (Prism emulation) — a naive
  // concat would produce "win32-arm64" and miss the x64 asset that
  // actually runs on those machines.
  const archMap = PLATFORM_ARCH_MAP[process.platform] ?? {};
  const platformKey = archMap[process.arch];
  const assetName = platformKey ? PLATFORM_ASSET_MAP[platformKey] : undefined;

  if (!platformKey || !assetName) {
    error(`Unsupported platform: ${process.platform}-${process.arch}`);
    return null;
  }

  // Resolve version if not provided
  const rawTag = version ?? (await fetchLatestTag());
  if (!rawTag) {
    error("Could not determine latest release version.");
    return null;
  }
  // Normalize tag to always have the `v` prefix. GitHub release URLs and the
  // versioned cache directory both expect `v`-prefixed tags. Without this,
  // callers passing the bare version (e.g. `"0.25.1"`) construct broken
  // URLs (404) and split the cache layout.
  const tag = rawTag.startsWith("v") ? rawTag : `v${rawTag}`;

  // Version-specific cache: ~/.cache/aft/bin/<tag>/aft
  const versionedCacheDir = join(getCacheDir(), tag);
  const binaryName = getBinaryName();
  const binaryPath = join(versionedCacheDir, binaryName);

  // Already cached for this version
  if (existsSync(binaryPath)) {
    return binaryPath;
  }

  const downloadUrl = `https://github.com/${REPO}/releases/download/${tag}/${assetName}`;
  const checksumUrl = `https://github.com/${REPO}/releases/download/${tag}/checksums.sha256`;

  log(`Downloading AFT binary (${tag}) for ${platformKey}...`);

  try {
    // Ensure versioned cache directory exists
    if (!existsSync(versionedCacheDir)) {
      mkdirSync(versionedCacheDir, { recursive: true });
    }

    // Download binary and checksum file in parallel
    const [binaryResponse, checksumResponse] = await Promise.all([
      fetch(downloadUrl, { redirect: "follow" }),
      fetch(checksumUrl, { redirect: "follow" }),
    ]);

    if (!binaryResponse.ok) {
      throw new Error(
        `HTTP ${binaryResponse.status}: ${binaryResponse.statusText} (${downloadUrl})`,
      );
    }

    const arrayBuffer = await binaryResponse.arrayBuffer();

    // Verify checksum - MANDATORY for security
    if (!checksumResponse.ok) {
      warn(
        `Checksum verification failed: no checksums.sha256 found for ${tag}. ` +
          "Binary download aborted for security reasons.",
      );
      return null;
    }

    const checksumText = await checksumResponse.text();
    const expectedHash = parseChecksumForAsset(checksumText, assetName);
    if (!expectedHash) {
      warn(
        `Checksum verification failed: checksums.sha256 found but no entry for ${assetName}. ` +
          "Binary download aborted for security reasons.",
      );
      return null;
    }

    const { createHash } = await import("node:crypto");
    const actualHash = createHash("sha256").update(Buffer.from(arrayBuffer)).digest("hex");
    if (actualHash !== expectedHash) {
      throw new Error(
        `Checksum mismatch for ${assetName}: expected ${expectedHash}, got ${actualHash}. ` +
          "The binary may have been tampered with.",
      );
    }
    log(`Checksum verified (SHA-256: ${actualHash.slice(0, 16)}...)`);

    // Write to a temp file first, then rename (atomic-ish)
    const tmpPath = `${binaryPath}.tmp`;
    const { writeFileSync } = await import("node:fs");
    writeFileSync(tmpPath, Buffer.from(arrayBuffer));

    // Make executable
    if (process.platform !== "win32") {
      chmodSync(tmpPath, 0o755);
    }

    // Atomic rename
    const { renameSync } = await import("node:fs");
    renameSync(tmpPath, binaryPath);

    log(`AFT binary ready at ${binaryPath}`);
    return binaryPath;
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    error(`Failed to download AFT binary: ${msg}`);

    // Clean up partial download
    const tmpPath = `${binaryPath}.tmp`;
    if (existsSync(tmpPath)) {
      try {
        unlinkSync(tmpPath);
      } catch {
        // ignore cleanup failure
      }
    }

    return null;
  }
}

/**
 * Ensure the AFT binary is available: check cache, then download if needed.
 * This is the main entry point called by the resolver.
 *
 * @param version - Git tag (e.g. `"v0.25.1"` or `"0.25.1"` — both accepted).
 *   Normalized to a `v`-prefixed tag internally so the on-disk cache layout
 *   stays consistent regardless of caller convention.
 */
export async function ensureBinary(version?: string): Promise<string | null> {
  if (version) {
    // Normalize tag for cache lookup so a caller passing the bare version
    // (e.g. `"0.25.1"`) finds the same cache entry that `downloadBinary`
    // and `findBinarySync` write to (`~/.cache/aft/bin/v0.25.1/aft`).
    const tag = version.startsWith("v") ? version : `v${version}`;

    // When a specific version is requested, ONLY check the versioned cache.
    // Do NOT fall back to legacy flat cache — it may contain a different version,
    // causing an infinite spawn-check-replace loop.
    const versionCached = getCachedBinaryPath(tag);
    if (versionCached) {
      log(`Found cached binary for ${tag}: ${versionCached}`);
      return versionCached;
    }
    log(`No cached binary for ${tag}, downloading...`);
    return downloadBinary(tag);
  }
  // No version requested — download latest.
  log("No cached binary found, downloading latest...");
  return downloadBinary();
}

/**
 * Parse a checksums.sha256 file (GNU coreutils format) and return the hash
 * for the given asset name, or null if not found.
 *
 * Expected format: `<hex-hash>  <filename>` (two spaces between hash and name)
 */
function parseChecksumForAsset(checksumText: string, assetName: string): string | null {
  for (const line of checksumText.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    // Format: "abc123...  aft-darwin-arm64"
    const match = trimmed.match(/^([0-9a-f]{64})\s+(.+)$/);
    if (match && match[2] === assetName) {
      return match[1];
    }
  }
  return null;
}

/** Fetch the latest release tag from GitHub API. */
async function fetchLatestTag(): Promise<string | null> {
  try {
    const response = await fetch(`https://api.github.com/repos/${REPO}/releases/latest`, {
      headers: { Accept: "application/vnd.github.v3+json" },
    });
    if (!response.ok) return null;
    const data = (await response.json()) as { tag_name?: string };
    return data.tag_name ?? null;
  } catch {
    return null;
  }
}
