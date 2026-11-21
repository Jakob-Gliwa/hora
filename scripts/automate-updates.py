#!/usr/bin/env python3
"""Trusted-main automation. Never checks out or executes pull-request code.

Merges Dependabot pull requests whose diff passes a policy below, incompatible
crate upgrades included: the required checks decide. CI on main
then publishes ghcr.io/jakob-gliwa/hora:main; Unraid pulls it on request only.
"""
import base64
import json
import os
import re
import subprocess
import tomllib

REPO = "Jakob-Gliwa/hora"
REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"


def version(value):
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", value):
        raise ValueError(f"Not a stable version: {value}")
    return tuple(map(int, value.split(".")))


def plain(value):
    """Build metadata (e.g. toml's +spec-1.1.0) carries no semver meaning."""
    return value.split("+", 1)[0]


def compatible_line(value):
    """Cargo's semver-compatible release line: 1.x.y, 0.3.y or exactly 0.0.z."""
    major, minor, patch = (re.split(r"[.-]", plain(value)) + ["", ""])[:3]
    return (major,) if major != "0" else (major, minor) if minor != "0" else (major, minor, patch)


def compatible_update(before, after):
    """Same crates and sources, stable upgrades within Cargo's compatible release lines.

    Pre-releases, downgrades, incompatible bumps and new or removed crates need review.
    """
    def packages(data):
        result = {}
        for package in data["package"]:
            key = (package["name"], package.get("source", ""), compatible_line(package["version"]))
            if key in result:
                raise ValueError("Multiple versions of a crate need review")
            result[key] = package
        return result

    try:
        old, new = packages(before), packages(after)
        if old.keys() != new.keys():
            return False
        changed = False
        for key, current in new.items():
            previous = old[key]
            if current == previous:
                continue
            if key[1] != REGISTRY:
                return False
            if version(plain(current["version"])) <= version(plain(previous["version"])):
                return False
            changed = True
        return changed
    except (KeyError, TypeError, ValueError):
        return False


def upgrade_only(before, after):
    """Any stable crates.io upgrade, incompatible ones included; CI decides the rest.

    New or dropped transitive crates are fine when they come from crates.io.
    Downgrades, pre-releases, other sources and workspace member changes need review.
    """
    def versions(data):
        result = {}
        for package in data["package"]:
            result.setdefault((package["name"], package.get("source", "")), {})[
                package["version"]] = package
        return result

    def identical(a, b):
        """Same package; its dependency list may name other crates' new versions."""
        return ({k: v for k, v in a.items() if k != "dependencies"}
                == {k: v for k, v in b.items() if k != "dependencies"})

    try:
        old, new = versions(before), versions(after)
        changed = False
        for key in old.keys() | new.keys():
            previous, current = old.get(key, {}), new.get(key, {})
            if previous == current:
                continue
            # A version present on both sides must keep its checksum and source.
            if any(not identical(previous[v], current[v]) for v in previous.keys() & current.keys()):
                return False
            if previous.keys() == current.keys():
                continue  # Only dependency lists moved, e.g. a member's requirement bump.
            if key[1] != REGISTRY:
                return False
            removed = previous.keys() - current.keys()
            for added in current.keys() - previous.keys():
                # Within a release line only upward; a new line must not undercut
                # the oldest version locked before. Lines may also just disappear.
                line = [version(plain(v)) for v in removed
                        if compatible_line(v) == compatible_line(added)]
                floor = max(line) if line else min(
                    (version(plain(v)) for v in previous), default=(-1,))
                if version(plain(added)) < floor or line and version(plain(added)) == floor:
                    return False
            changed = True
        return changed
    except (KeyError, TypeError, ValueError):
        return False


DEPENDENCY_TABLES = {"dependencies", "dev-dependencies", "build-dependencies"}


def requirement(value):
    """A plain, stable version requirement such as "0.5.2" or "^1.4"."""
    return isinstance(value, str) and re.fullmatch(r"[\^~=]?[0-9]+(\.[0-9]+){0,2}", value)


def version_requirements_only(before, after, path=()):
    """Manifests may differ only in the version requirement of a dependency."""
    if isinstance(before, dict) and isinstance(after, dict):
        if before.keys() != after.keys():
            return False
        return all(version_requirements_only(before[k], after[k], (*path, k)) for k in before)
    if before == after:
        return True
    in_table = len(path) >= 2 and path[-2] in DEPENDENCY_TABLES
    is_version = len(path) >= 3 and path[-3] in DEPENDENCY_TABLES and path[-1] == "version"
    return (in_table or is_version) and requirement(before) and requirement(after)


FROM_DIGEST = re.compile(r"FROM (\S+)@sha256:([0-9a-f]{64})(\s.*)?")


def digest_only(before, after):
    """Base images keep their exact name and tag; only pinned digests may change."""
    old, new = before.splitlines(), after.splitlines()
    if len(old) != len(new):
        return False
    changed = False
    for previous, current in zip(old, new):
        if previous == current:
            continue
        a, b = FROM_DIGEST.fullmatch(previous), FROM_DIGEST.fullmatch(current)
        if not a or not b or a[1] != b[1] or a[3] != b[3] or a[2] == b[2]:
            return False
        changed = True
    return changed


def gh(*args):
    return subprocess.check_output(["gh", *args], text=True)


def api(path, method="GET", payload=None):
    command = ["gh", "api", f"repos/{REPO}/{path}", "--method", method]
    if payload is not None:
        command += ["--input", "-"]
    result = subprocess.run(command, input=json.dumps(payload) if payload is not None else None,
                            capture_output=True, text=True, check=True)
    return json.loads(result.stdout) if result.stdout.strip() else None


def pages(path):
    return [item for page in json.loads(gh("api", f"repos/{REPO}/{path}", "--paginate", "--slurp"))
            for item in page]


def content(path, sha):
    return base64.b64decode(api(f"contents/{path}?ref={sha}")["content"]).decode()


def lockfile(sha):
    return tomllib.loads(content("Cargo.lock", sha))


# Files Dependabot may change unattended, each with the policy its diff must satisfy.
UNATTENDED = {
    "Cargo.lock": lambda base, head: compatible_update(lockfile(base), lockfile(head)),
    "Dockerfile": lambda base, head: digest_only(content("Dockerfile", base),
                                                 content("Dockerfile", head)),
}


def manifest(path, sha):
    return tomllib.loads(content(path, sha))


def unattended(filenames, base, head):
    """Route a pull request's files to the policy their diff must satisfy."""
    if len(filenames) == 1 and filenames[0] in UNATTENDED:
        return UNATTENDED[filenames[0]](base, head)
    # A requirement bump (incompatible versions included): manifests change only
    # version requirements, the lockfile only upgrades. The required checks,
    # cargo deny among them, then decide whether it merges.
    manifests = [f for f in filenames if f == "Cargo.toml" or f.endswith("/Cargo.toml")]
    if "Cargo.lock" not in filenames or not manifests or len(manifests) + 1 != len(filenames):
        return None
    return (all(version_requirements_only(manifest(f, base), manifest(f, head)) for f in manifests)
            and upgrade_only(lockfile(base), lockfile(head)))


def dispatch(workflow, ref):
    api(f"actions/workflows/{workflow}/dispatches", "POST", {"ref": ref})
    print(f"Dispatched {workflow} on {ref}", flush=True)


def ensure_main_ci():
    """GITHUB_TOKEN merges trigger no push workflow; catch up if a dispatch was lost."""
    sha = api("git/ref/heads/main")["object"]["sha"]
    runs = api(f"actions/workflows/ci.yml/runs?head_sha={sha}&per_page=100")["workflow_runs"]
    if not any(r["head_branch"] == "main" for r in runs):
        dispatch("ci.yml", "main")


def merge_updates():
    for pr in pages("pulls?state=open&base=main&per_page=100"):
        if (pr["user"]["login"] != "dependabot[bot]" or pr["user"]["type"] != "Bot"
                or pr["draft"] or not pr["head"].get("repo")
                or pr["head"]["repo"]["full_name"] != REPO):
            continue
        number, sha = pr["number"], pr["head"]["sha"]
        files = pages(f"pulls/{number}/files?per_page=100")
        verdict = (unattended([f["filename"] for f in files], pr["base"]["sha"], sha)
                   if all(f["status"] == "modified" for f in files) else None)
        if verdict is None:
            print(f"PR #{number}: manual review (files)")
            continue
        if not verdict:
            print(f"PR #{number}: manual review (dependency changes)")
            continue
        detail = api(f"pulls/{number}")
        if detail["head"]["sha"] != sha or detail["mergeable_state"] != "clean":
            print(f"PR #{number}: waiting for required checks / rebase")
            continue
        # The main ruleset enforces the required CI checks. The expected SHA
        # prevents merging a different revision after inspection.
        result = api(f"pulls/{number}/merge", "PUT", {"sha": sha, "merge_method": "squash"})
        if not result["merged"]:
            raise RuntimeError(f"PR #{number} was not merged")
        print(f"Merged dependency update #{number}", flush=True)
        dispatch("ci.yml", "main")
        return True  # Serialize updates: test and publish the resulting main first.
    return False


def main():
    if os.environ.get("GITHUB_REPOSITORY") != REPO:
        raise RuntimeError("Unexpected repository")
    ensure_main_ci()
    merge_updates()


if __name__ == "__main__":
    main()
