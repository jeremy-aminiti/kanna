#!/usr/bin/env python3
"""Resolve the pinned Ubuntu Noble development sysroot for Kanna.

The resolver reads package indexes from one immutable Ubuntu snapshot and
writes every selected .deb URL and SHA-256. Bazel consumes the lock without
performing dependency resolution, so a later mirror change cannot alter the
graph under the same source revision.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import lzma
import re
import sys
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable

SNAPSHOT = "20260909T000000Z"
SNAPSHOT_ROOT = f"https://snapshot.ubuntu.com/ubuntu/{SNAPSHOT}"
RELEASE = "noble"
COMPONENTS = ("main", "universe")
ARCHITECTURES = ("amd64", "arm64")

# These are headers/pkg-config surfaces compiled against by Tauri and the
# Linux-only desktop closure. Runtime dependencies are resolved recursively;
# build utilities are deliberately not executed from the target sysroot.
DEVELOPMENT_SEEDS = (
    "libayatana-appindicator3-dev",
    "libgtk-3-dev",
    "libsoup-3.0-dev",
    "libwebkit2gtk-4.1-dev",
)


def runtime_policy_seeds() -> tuple[str, ...]:
    policy_path = Path(__file__).with_name("runtime-policy.json")
    policy = json.loads(policy_path.read_text(encoding="utf-8"))
    return tuple(entry["package"] for entry in policy["allowedRuntimeLibraries"])


SEEDS = tuple(sorted(set(DEVELOPMENT_SEEDS + runtime_policy_seeds())))


@dataclass(frozen=True)
class Package:
    name: str
    version: str
    architecture: str
    filename: str
    sha256: str
    size: int
    depends: str
    pre_depends: str
    provides: tuple[str, ...]

    def lock_entry(self) -> dict[str, object]:
        return {
            "architecture": self.architecture,
            "name": self.name,
            "sha256": self.sha256,
            "size": self.size,
            "url": f"{SNAPSHOT_ROOT}/{self.filename}",
            "version": self.version,
        }


def parse_control(text: str) -> list[dict[str, str]]:
    """Parse Debian control paragraphs, including continuation lines."""
    paragraphs: list[dict[str, str]] = []
    for raw in text.split("\n\n"):
        fields: dict[str, str] = {}
        current: str | None = None
        for line in raw.splitlines():
            if line.startswith((" ", "\t")) and current is not None:
                fields[current] += "\n" + line[1:]
                continue
            if ":" not in line:
                current = None
                continue
            current, value = line.split(":", 1)
            fields[current] = value.lstrip()
        if fields:
            paragraphs.append(fields)
    return paragraphs


def _split_relations(value: str, separator: str) -> list[str]:
    parts: list[str] = []
    start = 0
    depths = {"(": 0, "[": 0, "<": 0}
    closing = {")": "(", "]": "[", ">": "<"}
    for index, char in enumerate(value):
        if char in depths:
            depths[char] += 1
        elif char in closing and depths[closing[char]]:
            depths[closing[char]] -= 1
        elif char == separator and not any(depths.values()):
            parts.append(value[start:index].strip())
            start = index + 1
    tail = value[start:].strip()
    if tail:
        parts.append(tail)
    return parts


def _architecture_matches(expression: str, architecture: str) -> bool:
    match = re.search(r"\[([^]]+)\]", expression)
    if not match:
        return True
    terms = match.group(1).split()
    positives = [term for term in terms if not term.startswith("!")]
    negatives = [term[1:] for term in terms if term.startswith("!")]

    def matches(term: str) -> bool:
        return term == architecture or term in ("any", "linux-any") or (
            term.endswith("-any") and term.removesuffix("-any") == architecture
        )

    return not any(matches(term) for term in negatives) and (
        not positives or any(matches(term) for term in positives)
    )


def relation_alternatives(value: str, architecture: str) -> list[list[str]]:
    """Return applicable package-name alternatives for Depends syntax."""
    groups: list[list[str]] = []
    for group in _split_relations(value, ","):
        alternatives: list[str] = []
        for expression in _split_relations(group, "|"):
            if not _architecture_matches(expression, architecture):
                continue
            expression = re.sub(r"\([^)]*\)|\[[^]]*\]|<[^>]*>", "", expression).strip()
            name = expression.split(":", 1)[0].strip()
            if re.fullmatch(r"[a-z0-9][a-z0-9+.-]*", name):
                alternatives.append(name)
        if alternatives:
            groups.append(alternatives)
    return groups


def package_from_fields(fields: dict[str, str]) -> Package | None:
    required = ("Package", "Version", "Architecture", "Filename", "SHA256", "Size")
    if any(key not in fields for key in required):
        return None
    provides = tuple(
        alternative
        for group in relation_alternatives(fields.get("Provides", ""), fields["Architecture"])
        for alternative in group
    )
    return Package(
        name=fields["Package"],
        version=fields["Version"],
        architecture=fields["Architecture"],
        filename=fields["Filename"],
        sha256=fields["SHA256"],
        size=int(fields["Size"]),
        depends=fields.get("Depends", ""),
        pre_depends=fields.get("Pre-Depends", ""),
        provides=provides,
    )


def index_url(component: str, architecture: str) -> str:
    return f"{SNAPSHOT_ROOT}/dists/{RELEASE}/{component}/binary-{architecture}/Packages.xz"


def fetch(url: str) -> bytes:
    request = urllib.request.Request(url, headers={"User-Agent": "kanna-sysroot-resolver/1"})
    with urllib.request.urlopen(request) as response:
        return response.read()


def load_index(architecture: str) -> tuple[list[Package], list[dict[str, object]]]:
    packages: list[Package] = []
    indexes: list[dict[str, object]] = []
    for component in COMPONENTS:
        url = index_url(component, architecture)
        compressed = fetch(url)
        indexes.append({
            "component": component,
            "sha256": hashlib.sha256(compressed).hexdigest(),
            "size": len(compressed),
            "url": url,
        })
        content = lzma.decompress(compressed).decode("utf-8")
        packages.extend(
            package
            for fields in parse_control(content)
            if (package := package_from_fields(fields)) is not None
            and package.architecture in (architecture, "all")
        )
    return packages, indexes


def resolve_packages(packages: Iterable[Package], architecture: str) -> list[Package]:
    by_name: dict[str, Package] = {}
    providers: dict[str, list[Package]] = {}
    for package in packages:
        previous = by_name.get(package.name)
        # A target-specific package wins over an Architecture: all entry. The
        # pinned base release otherwise contains one candidate per name.
        if previous is None or (previous.architecture == "all" and package.architecture == architecture):
            by_name[package.name] = package
        for provided in package.provides:
            providers.setdefault(provided, []).append(package)

    selected: dict[str, Package] = {}
    pending = list(SEEDS)
    while pending:
        requested = pending.pop(0)
        if requested in selected:
            continue
        package = by_name.get(requested)
        if package is None:
            candidates = providers.get(requested, [])
            if len(candidates) != 1:
                names = ", ".join(sorted(candidate.name for candidate in candidates)) or "none"
                raise ValueError(f"{requested} has {len(candidates)} providers ({names})")
            package = candidates[0]
        selected[package.name] = package
        for relation in (package.pre_depends, package.depends):
            for alternatives in relation_alternatives(relation, architecture):
                choice = next(
                    (
                        name
                        for name in alternatives
                        if name in by_name or len(providers.get(name, [])) == 1
                    ),
                    None,
                )
                if choice is None:
                    raise ValueError(
                        f"{package.name} dependency {' | '.join(alternatives)} has no unambiguous candidate"
                    )
                pending.append(choice)
    return sorted(selected.values(), key=lambda package: (package.name, package.architecture))


def build_lock(
    architecture: str,
    packages: Iterable[Package],
    indexes: Iterable[dict[str, object]] = (),
) -> dict[str, object]:
    resolved = resolve_packages(packages, architecture)
    return {
        "architecture": architecture,
        "components": list(COMPONENTS),
        "formatVersion": 1,
        "indexes": list(indexes),
        "packages": [package.lock_entry() for package in resolved],
        "release": RELEASE,
        "seeds": list(SEEDS),
        "snapshot": SNAPSHOT,
        "snapshotRoot": SNAPSHOT_ROOT,
    }


def canonical_json(value: object) -> str:
    return json.dumps(value, indent=2, sort_keys=True) + "\n"


def validate_lock(lock: dict[str, object], path: Path) -> None:
    architecture = lock.get("architecture")
    if architecture not in ARCHITECTURES:
        raise ValueError(f"{path}: unsupported architecture {architecture!r}")
    packages, indexes = load_index(str(architecture))
    expected = build_lock(str(architecture), packages, indexes)
    if canonical_json(lock) != canonical_json(expected):
        raise ValueError(f"{path}: lock differs from {SNAPSHOT} resolution; regenerate it")


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--architecture", choices=ARCHITECTURES)
    group.add_argument("--check", type=Path)
    parser.add_argument("--output", type=Path)
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    if args.check:
        lock = json.loads(args.check.read_text(encoding="utf-8"))
        validate_lock(lock, args.check)
        digest = hashlib.sha256(canonical_json(lock).encode()).hexdigest()
        print(f"{args.check}: OK ({len(lock['packages'])} packages, lock sha256 {digest})")
        return 0
    if args.output is None:
        raise ValueError("--architecture requires --output")
    packages, indexes = load_index(args.architecture)
    lock = build_lock(args.architecture, packages, indexes)
    args.output.write_text(canonical_json(lock), encoding="utf-8")
    print(f"wrote {args.output} ({len(lock['packages'])} packages)")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main(sys.argv[1:]))
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1)
