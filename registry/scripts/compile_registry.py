#!/usr/bin/env python3
"""Compile registry/plugins/*.json into a static dist/index.min.json.

For every enabled manifest this script:
  1. validates the manifest against registry/schema/plugin.schema.json,
  2. resolves the latest GitHub release of `source.github`,
  3. picks one asset per platform using the manifest's regex `asset_patterns`,
  4. records the download URL, size and SHA-256 (from the GitHub digest when present,
     otherwise by streaming the asset once),
  5. merges entries from `federated_feed_url` / `vendor_manifest_url` feeds.

Failures are isolated per plugin: a broken upstream becomes a warning, not a broken index.
The script refuses to publish an empty index so a GitHub outage cannot wipe the catalog.

Usage:
    python registry/scripts/compile_registry.py --plugins registry/plugins \
        --schema registry/schema/plugin.schema.json --out dist

Environment:
    GITHUB_TOKEN  optional; raises the GitHub API rate limit (60 -> 5000 requests/hour).
"""
from __future__ import annotations

import argparse
import asyncio
import hashlib
import json
import os
import re
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import aiohttp
from jsonschema import Draft202012Validator

GITHUB_API = "https://api.github.com"
USER_AGENT = "patchbay-registry/1.0"
PLATFORMS = ("win_x64", "mac_universal", "linux_x64")
SCHEMA_VERSION = 1
MAX_CONCURRENCY = 6
ID_PATTERN = re.compile(r"[a-z0-9][a-z0-9-]{1,62}")
AUTH_TYPES = ("none", "bearer_token", "license_key_header")
FEED_REQUIRED = ("id", "name", "vendor", "category", "latest", "assets")
KIND_SUFFIXES = (
    (".tar.gz", "tar.gz"),
    (".tgz", "tar.gz"),
    (".zip", "zip"),
    (".dmg", "dmg"),
    (".exe", "exe"),
    (".msi", "msi"),
    (".pkg", "pkg"),
)


class CompileError(RuntimeError):
    """A manifest is invalid. The whole build stops, because the catalog would be wrong."""


class Report:
    """Collects per-plugin warnings. They are printed and embedded in the index."""

    def __init__(self) -> None:
        self.warnings: list[dict[str, str]] = []

    def warn(self, plugin_id: str, message: str) -> None:
        print(f"[warn] {plugin_id}: {message}", file=sys.stderr)
        self.warnings.append({"id": plugin_id, "message": message})


def kind_for(name: str) -> str:
    lower = name.lower()
    for suffix, kind in KIND_SUFFIXES:
        if lower.endswith(suffix):
            return kind
    return "unknown"


def normalize_version(tag: str) -> str:
    return re.sub(r"^v(?=\d)", "", tag.strip())


def load_manifests(plugins_dir: Path, schema: dict[str, Any]) -> list[dict[str, Any]]:
    validator = Draft202012Validator(schema)
    manifests: list[dict[str, Any]] = []
    seen: set[str] = set()
    for path in sorted(plugins_dir.glob("*.json")):
        data = json.loads(path.read_text(encoding="utf-8"))
        errors = sorted(validator.iter_errors(data), key=lambda e: [str(p) for p in e.absolute_path])
        if errors:
            where = "/".join(str(p) for p in errors[0].absolute_path) or "<root>"
            raise CompileError(f"{path.name}: {where}: {errors[0].message}")
        if data["id"] != path.stem:
            raise CompileError(f"{path.name}: id '{data['id']}' must match the file name")
        if data["id"] in seen:
            raise CompileError(f"duplicate plugin id '{data['id']}'")
        seen.add(data["id"])
        manifests.append(data)
    if not manifests:
        raise CompileError(f"no manifests found in {plugins_dir}")
    return manifests


def github_headers(token: str | None) -> dict[str, str]:
    headers = {
        "Accept": "application/vnd.github+json",
        "User-Agent": USER_AGENT,
        "X-GitHub-Api-Version": "2022-11-28",
    }
    if token:
        headers["Authorization"] = f"Bearer {token}"
    return headers


async def get_json(session: aiohttp.ClientSession, url: str, headers: dict[str, str]) -> Any | None:
    async with session.get(url, headers=headers) as resp:
        if resp.status == 404:
            return None
        resp.raise_for_status()
        return await resp.json()


PRERELEASE_TAG = re.compile(r"(?i)(nightly|alpha|beta|dev)")


def is_stable(release: dict[str, Any], allow_prerelease: bool) -> bool:
    """Stable means: not a draft, not flagged as pre-release, and the tag is not nightly/alpha/beta/dev.

    A manifest can opt back in with "allow_prerelease": true (e.g. a project whose only release is tagged
    as a nightly build)."""
    if release.get("draft"):
        return False
    if allow_prerelease:
        return True
    return not release.get("prerelease") and not PRERELEASE_TAG.search(release.get("tag_name", ""))


async def latest_release(
    session: aiohttp.ClientSession,
    repo: str,
    token: str | None,
    allow_prerelease: bool,
    release_tag: str | None = None,
) -> dict[str, Any] | None:
    headers = github_headers(token)
    if release_tag:
        # Pinned release: the manifest names the exact tag to publish from.
        return await get_json(session, f"{GITHUB_API}/repos/{repo}/releases/tags/{release_tag}", headers)
    releases = await get_json(session, f"{GITHUB_API}/repos/{repo}/releases?per_page=30", headers) or []
    for candidate in releases:  # GitHub lists newest first
        if is_stable(candidate, allow_prerelease):
            return candidate
    return None


async def asset_sha256(session: aiohttp.ClientSession, asset: dict[str, Any]) -> str:
    digest = asset.get("digest")
    if isinstance(digest, str) and digest.startswith("sha256:"):
        return digest.split(":", 1)[1].lower()
    hasher = hashlib.sha256()
    async with session.get(asset["browser_download_url"], headers={"User-Agent": USER_AGENT}) as resp:
        resp.raise_for_status()
        async for chunk in resp.content.iter_chunked(1 << 16):
            hasher.update(chunk)
    return hasher.hexdigest()


def base_fields(manifest: dict[str, Any]) -> dict[str, Any]:
    return {
        "id": manifest["id"],
        "name": manifest["name"],
        "vendor": manifest["vendor"],
        "category": manifest["category"],
        "license": manifest["license"],
        "open_source": manifest["open_source"],
        "homepage": manifest.get("homepage"),
        "description": manifest.get("description"),
        "about": manifest.get("about"),
        "tags": manifest.get("tags", []),
        "screenshot": manifest.get("screenshot"),
        "install_strategy": manifest["install_strategy"],
        "silent_flags": manifest.get("silent_flags"),
        "auth_type": manifest["auth_type"],
        "auth": manifest.get("auth"),
        "formats": manifest.get("formats", []),
    }


async def build_github_entry(
    session: aiohttp.ClientSession,
    manifest: dict[str, Any],
    token: str | None,
    report: Report,
) -> dict[str, Any] | None:
    plugin_id = manifest["id"]
    repo = manifest["source"]["github"]
    release = await latest_release(
        session, repo, token, manifest.get("allow_prerelease", False), manifest.get("release_tag")
    )
    if release is None:
        report.warn(plugin_id, f"{repo} has no stable release (pre-releases skipped)")
        return None

    api_assets = release.get("assets", [])
    assets: dict[str, Any] = {}
    matched_names: list[str] = []
    for platform, pattern in manifest["source"]["asset_patterns"].items():
        regex = re.compile(pattern)
        match = next((a for a in api_assets if regex.fullmatch(a["name"])), None)
        if match is None:
            report.warn(plugin_id, f"no {platform} asset matches /{pattern}/ in {release.get('tag_name')}")
            continue
        matched_names.append(match["name"])
        assets[platform] = {
            "name": match["name"],
            "url": match["browser_download_url"],
            "size": match.get("size"),
            "sha256": await asset_sha256(session, match),
            "kind": kind_for(match["name"]),
        }
    if not assets:
        return None

    version = normalize_version(release["tag_name"])
    version_regex = manifest.get("version_regex")
    if version_regex and matched_names:
        found = re.search(version_regex, matched_names[0])
        if found and found.groupdict().get("version"):
            version = found.group("version")
            # Date builds (2026-11-15) become dotted (2026.11.15) so they sort as versions.
            if re.fullmatch(r"\d{4}-\d{2}-\d{2}", version):
                version = version.replace("-", ".")

    return {
        **base_fields(manifest),
        "latest": {
            "version": version,
            "published_at": release.get("published_at"),
            "release_url": release.get("html_url"),
        },
        "assets": assets,
        "source": f"github:{repo}",
    }


def feed_entry(item: dict[str, Any], origin: str, report: Report) -> dict[str, Any] | None:
    """Normalise one item of a federated / vendor feed into the index shape."""
    plugin_id = str(item.get("id", ""))
    if not all(key in item for key in FEED_REQUIRED) or not ID_PATTERN.fullmatch(plugin_id):
        report.warn(origin, f"feed item skipped: needs {FEED_REQUIRED} and a valid id")
        return None
    if item.get("auth_type", "none") not in AUTH_TYPES:
        report.warn(plugin_id, "feed item skipped: unknown auth_type")
        return None
    assets: dict[str, Any] = {}
    for platform, asset in item["assets"].items():
        if platform not in PLATFORMS or not isinstance(asset, dict):
            continue
        url = str(asset.get("url", ""))
        sha = str(asset.get("sha256", "")).lower()
        if not url.startswith("https://") or not re.fullmatch(r"[0-9a-f]{64}", sha):
            report.warn(plugin_id, f"feed asset for {platform} skipped: needs https url and sha256")
            continue
        name = str(asset.get("name") or url.rsplit("/", 1)[-1])
        assets[platform] = {
            "name": name,
            "url": url,
            "size": asset.get("size"),
            "sha256": sha,
            "kind": kind_for(name),
        }
    if not assets:
        report.warn(plugin_id, "feed item skipped: no verifiable assets")
        return None
    latest = item["latest"]
    return {
        "id": plugin_id,
        "name": item["name"],
        "vendor": item["vendor"],
        "category": item["category"],
        "license": item.get("license", "unknown"),
        "open_source": bool(item.get("open_source", False)),
        "homepage": item.get("homepage"),
        "description": item.get("description"),
        "about": item.get("about"),
        "tags": item.get("tags", []),
        "screenshot": item.get("screenshot") if str(item.get("screenshot", "")).startswith("https://raw.githubusercontent.com/") else None,
        "install_strategy": item.get("install_strategy", "bundle_extract"),
        "silent_flags": item.get("silent_flags"),
        "auth_type": item.get("auth_type", "none"),
        "auth": item.get("auth"),
        "formats": item.get("formats", []),
        "latest": {
            "version": str(latest["version"]),
            "published_at": latest.get("published_at"),
            "release_url": latest.get("release_url"),
        },
        "assets": assets,
        "source": origin,
    }


async def load_feed(
    session: aiohttp.ClientSession, url: str, plugin_id: str, report: Report
) -> list[dict[str, Any]]:
    async with session.get(url, headers={"User-Agent": USER_AGENT}) as resp:
        resp.raise_for_status()
        data = await resp.json(content_type=None)
    items = data.get("plugins", []) if isinstance(data, dict) else data
    entries: list[dict[str, Any]] = []
    for item in items:
        if not isinstance(item, dict):
            report.warn(plugin_id, "feed item skipped: not an object")
            continue
        entry = feed_entry(item, f"feed:{url}", report)
        if entry is not None:
            entries.append(entry)
    return entries


async def resolve_manifest(
    session: aiohttp.ClientSession,
    semaphore: asyncio.Semaphore,
    manifest: dict[str, Any],
    token: str | None,
    report: Report,
) -> list[dict[str, Any]]:
    plugin_id = manifest["id"]
    entries: list[dict[str, Any]] = []
    async with semaphore:
        if manifest.get("source"):
            try:
                entry = await build_github_entry(session, manifest, token, report)
                if entry is not None:
                    entries.append(entry)
            except Exception as exc:  # one broken upstream must not break the whole index
                report.warn(plugin_id, f"github lookup failed: {exc}")
        for key in ("federated_feed_url", "vendor_manifest_url"):
            url = manifest.get(key)
            if url:
                try:
                    entries.extend(await load_feed(session, url, plugin_id, report))
                except Exception as exc:
                    report.warn(plugin_id, f"{key} failed: {exc}")
    if not entries:
        report.warn(plugin_id, "no downloadable build resolved")
    return entries


async def compile_index(
    manifests: list[dict[str, Any]], token: str | None, report: Report
) -> list[dict[str, Any]]:
    for manifest in manifests:
        if not manifest.get("enabled", True):
            print(f"[skip] {manifest['id']}: disabled ({manifest.get('notes') or 'no notes'})", file=sys.stderr)
    active = [m for m in manifests if m.get("enabled", True)]
    semaphore = asyncio.Semaphore(MAX_CONCURRENCY)
    timeout = aiohttp.ClientTimeout(total=None, sock_connect=30, sock_read=120)
    async with aiohttp.ClientSession(timeout=timeout) as session:
        results = await asyncio.gather(
            *(resolve_manifest(session, semaphore, m, token, report) for m in active)
        )
    seen: set[str] = set()
    plugins: list[dict[str, Any]] = []
    for entries in results:
        for entry in entries:
            if entry["id"] in seen:
                report.warn(entry["id"], f"duplicate entry from {entry['source']} ignored")
                continue
            seen.add(entry["id"])
            plugins.append(entry)
    plugins.sort(key=lambda p: p["id"])
    return plugins


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Compile Patchbay plugin manifests into index.min.json.")
    parser.add_argument("--plugins", type=Path, default=Path("registry/plugins"))
    parser.add_argument("--schema", type=Path, default=Path("registry/schema/plugin.schema.json"))
    parser.add_argument("--out", type=Path, default=Path("dist"))
    parser.add_argument("--strict", action="store_true", help="exit non-zero if any warning was produced")
    args = parser.parse_args(argv)

    schema = json.loads(args.schema.read_text(encoding="utf-8"))
    report = Report()
    try:
        manifests = load_manifests(args.plugins, schema)
        token = os.environ.get("GITHUB_TOKEN") or None
        plugins = asyncio.run(compile_index(manifests, token, report))
    except CompileError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2

    if not plugins:
        print("error: no plugin resolved; refusing to publish an empty index", file=sys.stderr)
        return 3
    if args.strict and report.warnings:
        print(f"error: {len(report.warnings)} warning(s) in strict mode", file=sys.stderr)
        return 4

    index = {
        "schema_version": SCHEMA_VERSION,
        "generated_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "plugins": plugins,
        "warnings": report.warnings,
    }
    args.out.mkdir(parents=True, exist_ok=True)
    (args.out / "index.min.json").write_text(
        json.dumps(index, separators=(",", ":"), ensure_ascii=False), encoding="utf-8"
    )
    (args.out / ".nojekyll").write_text("", encoding="utf-8")
    print(f"wrote {len(plugins)} plugin(s) to {args.out / 'index.min.json'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
