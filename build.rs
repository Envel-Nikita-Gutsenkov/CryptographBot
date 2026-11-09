//! Stamps the binary with a build hash and build timestamp.
//!
//! `/ver` (and `cryptograph version`) used to read `src/bot/version.txt`,
//! which only `build_dist.ps1` regenerated. Docker builds copy just `src/`,
//! so the file stayed `dev` forever. This script resolves the hash at compile
//! time instead, from the first source that works:
//!
//!   1. `GIT_HASH` / `CRYPTOGRAPH_BUILD_HASH` env (docker build-arg, CI)
//!   2. `git rev-parse --short HEAD` (normal local build)
//!   3. `src/bot/version.txt` if it holds a real value (legacy packaging)
//!   4. `src-<digest>` over all sources — still uniquely identifies the code
//!
//! The result is written to `$OUT_DIR/build_info.txt` as two lines:
//! the hash and the build time (unix seconds).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Where the fallback digest starts, so `src-...` never looks like a git hash.
const DIGEST_PREFIX: &str = "src-";

fn main() {
    let manifest = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is always set by cargo"),
    );

    // Re-run when anything that can change the answer changes.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-env-changed=GIT_HASH");
    println!("cargo:rerun-if-env-changed=CRYPTOGRAPH_BUILD_HASH");

    let hash = env_hash()
        .or_else(|| git_hash(&manifest))
        .or_else(|| legacy_version_file(&manifest))
        .unwrap_or_else(|| source_digest(&manifest));

    let built_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR is always set by cargo"));
    let target = out_dir.join("build_info.txt");
    fs::write(&target, format!("{hash}\n{built_at}\n"))
        .unwrap_or_else(|e| panic!("cannot write {}: {e}", target.display()));
}

/// Explicit override, e.g. `GIT_HASH=$(git rev-parse --short HEAD) docker compose build`.
fn env_hash() -> Option<String> {
    ["GIT_HASH", "CRYPTOGRAPH_BUILD_HASH"]
        .iter()
        .filter_map(|key| std::env::var(key).ok())
        .map(|v| v.trim().to_string())
        .find(|v| is_meaningful(v))
}

/// `git rev-parse --short HEAD` — works for local builds in a real checkout.
fn git_hash(manifest: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(manifest)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let hash = String::from_utf8_lossy(&out.stdout).trim().to_string();
    is_meaningful(&hash).then_some(hash)
}

/// Legacy `build_dist.ps1` packaging wrote the hash into this file.
fn legacy_version_file(manifest: &Path) -> Option<String> {
    let raw = fs::read_to_string(manifest.join("src/bot/version.txt")).ok()?;
    let value = raw.trim().to_string();
    is_meaningful(&value).then_some(value)
}

/// Content digest of everything that ends up in the binary, so even a
/// `.git`-less Docker build gets a hash that changes with the code.
fn source_digest(manifest: &Path) -> String {
    let mut files = Vec::new();
    collect_sources(&manifest.join("src"), &mut files);
    for extra in ["Cargo.toml", "Cargo.lock"] {
        let path = manifest.join(extra);
        if path.is_file() {
            files.push(path);
        }
    }
    files.sort();

    let mut hash = FNV_OFFSET;
    for path in &files {
        let rel = path.strip_prefix(manifest).unwrap_or(path);
        hash = fnv1a(rel.to_string_lossy().as_bytes(), hash);
        match fs::read(path) {
            Ok(bytes) => hash = fnv1a(&bytes, hash),
            // A file that vanished mid-build must still shift the digest.
            Err(_) => hash = fnv1a(b"<missing>", hash),
        }
    }
    format!("{DIGEST_PREFIX}{:07x}", hash & 0x0fff_ffff)
}

fn collect_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_sources(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

fn is_meaningful(value: &str) -> bool {
    !value.is_empty()
        && !value.eq_ignore_ascii_case("dev")
        && !value.eq_ignore_ascii_case("unknown")
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a(bytes: &[u8], mut hash: u64) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}
