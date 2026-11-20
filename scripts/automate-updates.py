#!/usr/bin/env python3
"""Trusted-main automation. Never checks out or executes pull-request code.

Merges Dependabot pull requests whose diff passes a policy below. CI on main
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
        if (len(files) != 1 or files[0]["filename"] not in UNATTENDED
                or files[0]["status"] != "modified"):
            print(f"PR #{number}: manual review (files)")
            continue
        if not UNATTENDED[files[0]["filename"]](pr["base"]["sha"], sha):
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
