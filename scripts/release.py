"""Validate and publish one manually dispatched, immutable Gopher release."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib

import bundle

REPO = "mbuvarp/gopher"
ROOT = Path(__file__).resolve().parent.parent


def run(*args, **kwargs):
    return subprocess.run(args, check=True, text=True, capture_output=True, **kwargs).stdout.strip()


class GitHub:
    def request(self, endpoint, method="GET", data=None):
        args = ["gh", "api", "--hostname", "github.com", f"repos/{REPO}/{endpoint}", "--method", method]
        if data is not None:
            args += ["--input", "-"]
        result = run(*args, input=json.dumps(data) if data is not None else None)
        return json.loads(result) if result else None

    def pages(self, endpoint):
        result = json.loads(run("gh", "api", "--hostname", "github.com", f"repos/{REPO}/{endpoint}",
                                "--paginate", "--slurp"))
        return [item for page in result for item in page]

    def upload(self, tag, files):
        run("gh", "release", "upload", tag, *map(str, files), "--repo", REPO)


def numeric_version(value):
    bundle.bundle_version(value)
    return tuple(map(int, value.split(".")))


def changelog_section(content, version):
    headings = list(re.finditer(r"^## \[([^\]\n]+)\](?: - \d{4}-\d{2}-\d{2})?\s*$", content, re.M))
    matches = [i for i, match in enumerate(headings) if match[1] == version]
    if len(matches) != 1:
        raise ValueError(f"Require exactly one CHANGELOG.md section: ## [{version}] - YYYY-MM-DD")
    index = matches[0]
    start = headings[index].end()
    # Stop at every level-two heading, even one with an invalid version format.
    end = re.search(r"^## ", content[start:], re.M)
    notes = content[start:start + end.start() if end else len(content)].strip()
    substantive = re.sub(r"<!--.*?-->", "", notes, flags=re.S)
    substantive = re.sub(r"^\s*#+.*$", "", substantive, flags=re.M).strip()
    if not substantive:
        raise ValueError("The release changelog section is empty")
    return notes + "\n"


def metadata(root):
    manifest = tomllib.loads((root / "Cargo.toml").read_text())["package"]
    version = manifest["version"]
    numeric_version(version)
    packages = tomllib.loads((root / "Cargo.lock").read_text())["package"]
    local = [p for p in packages if p["name"] == manifest["name"] and "source" not in p]
    if len(local) != 1 or local[0]["version"] != version:
        raise ValueError("Cargo.toml and Cargo.lock versions differ")
    return version, changelog_section((root / "CHANGELOG.md").read_text(), version)


def marker(version, sha):
    return f"<!-- gopher-release:v1 {version} {sha} -->"


def matching_draft(releases, version, sha):
    tag = "v" + version
    for release in releases:
        if (release["draft"] and release["tag_name"] != tag
                and f"<!-- gopher-release:v1 {version} " in (release.get("body") or "")):
            raise ValueError("Workflow draft has an unexpected tag; inspect it before retrying")
    matches = [r for r in releases if r["tag_name"] == tag]
    if len(matches) > 1:
        raise ValueError("Multiple releases claim this version")
    if not matches:
        return None
    release = matches[0]
    if (not release["draft"] or release.get("immutable") or release.get("prerelease")
            or release["target_commitish"] != sha
            or marker(version, sha) not in (release.get("body") or "")
            or release.get("author", {}).get("login") != "github-actions[bot]"):
        raise ValueError("Version already exists or draft belongs to another commit/workflow; manual recovery required")
    return release


def require_draft(release, version, sha):
    if matching_draft([release], version, sha) is None:
        raise ValueError("GitHub returned a draft with an unexpected tag; inspect it before retrying")
    return release


def validate_remote(api, version, sha):
    releases = api.pages("releases?per_page=100")
    for release in releases:
        if not release["draft"] and not release["prerelease"]:
            tag = release["tag_name"]
            if not tag.startswith("v"):
                raise ValueError(f"Unrecognized stable release tag: {tag}")
            if numeric_version(tag[1:]) >= numeric_version(version):
                raise ValueError("Cargo version must be newer than every published stable release")
    draft = matching_draft(releases, version, sha)
    refs = api.request(f"git/matching-refs/tags/v{version}")
    exact = [r for r in refs if r["ref"] == f"refs/tags/v{version}"]
    if exact:
        if not draft or len(exact) != 1 or (exact[0]["object"]["type"] != "commit" or exact[0]["object"]["sha"] != sha):
            raise ValueError("Tag collision; manual recovery required")
    return draft


def workflow_commit():
    if (os.environ.get("GITHUB_EVENT_NAME") != "workflow_dispatch"
            or os.environ.get("GITHUB_REF") != "refs/heads/main"
            or os.environ.get("GITHUB_REPOSITORY") != REPO):
        raise ValueError("Release workflow must be manually dispatched on mbuvarp/gopher main")
    sha = os.environ.get("GITHUB_SHA", "")
    expected = os.environ.get("GOPHER_EXPECTED_SHA", "")
    if expected and (not re.fullmatch(r"[0-9a-f]{40}", expected) or expected != sha):
        raise ValueError("Dispatched commit differs from the approved expected_sha; reassess main before retrying")
    if not re.fullmatch(r"[0-9a-f]{40}", sha) or run("git", "rev-parse", "HEAD") != sha:
        raise ValueError("Checkout does not match the dispatched commit")
    run("git", "fetch", "origin", "main")
    run("git", "merge-base", "--is-ancestor", sha, "origin/main")
    return sha


def artifact_names(version):
    archive = f"Gopher-{version}-macos-arm64.zip"
    return [archive, archive + ".sha256", "install.sh", "appcast.xml"]


def file_digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def write_receipt(directory, version, sha):
    names = artifact_names(version)
    hashes = {name: file_digest(directory / name) for name in names}
    archive = names[0]
    if (directory / names[1]).read_text() != f"{hashes[archive]}  {archive}\n":
        raise ValueError("Archive checksum does not match")
    (directory / "release.json").write_text(json.dumps({"version": version, "sha": sha, "hashes": hashes}))


def validate_receipt(directory, version, sha):
    receipt = json.loads((directory / "release.json").read_text())
    expected = {name: file_digest(directory / name) for name in artifact_names(version)}
    if receipt != {"version": version, "sha": sha, "hashes": expected}:
        raise ValueError("Release artifact does not match the dispatched build")
    return expected


def publish(api, directory, version, sha, notes):
    hashes = validate_receipt(directory, version, sha)
    draft = validate_remote(api, version, sha)
    body = notes + "\n" + marker(version, sha)
    if draft is None:
        draft = api.request("releases", "POST", {"tag_name": "v" + version, "target_commitish": sha,
                            "name": "v" + version, "body": body, "draft": True, "prerelease": False})
    release_id = draft["id"]
    # Recheck the server's draft immediately before touching its assets. Never
    # replace published assets or automatically delete conflicting releases/tags.
    current = api.request(f"releases/{release_id}")
    require_draft(current, version, sha)
    for asset in api.pages(f"releases/{release_id}/assets?per_page=100"):
        if asset["name"] not in hashes:
            raise ValueError("Unexpected draft asset; manual recovery required")
    # A rerun rebuilds signatures, so replace the complete unpublished asset set.
    for asset in api.pages(f"releases/{release_id}/assets?per_page=100"):
        require_draft(api.request(f"releases/{release_id}"), version, sha)
        api.request(f"releases/assets/{asset['id']}", "DELETE")
    require_draft(api.request(f"releases/{release_id}", "PATCH", {
        "tag_name": "v" + version, "target_commitish": sha, "body": body, "draft": True,
    }), version, sha)
    api.upload("v" + version, [directory / name for name in hashes])
    assets = api.pages(f"releases/{release_id}/assets?per_page=100")
    if (len(assets) != len(hashes) or {a["name"] for a in assets} != set(hashes)
            or any(a["state"] != "uploaded" or a.get("digest") != "sha256:" + hashes[a["name"]]
                   or a["size"] != (directory / a["name"]).stat().st_size for a in assets)):
        raise ValueError("Draft assets are incomplete or differ from the verified build")
    latest = validate_remote(api, version, sha)
    if latest is None or latest["id"] != release_id:
        raise ValueError("Draft changed during upload")
    published = api.request(f"releases/{release_id}", "PATCH", {
        "tag_name": "v" + version, "target_commitish": sha, "draft": False, "make_latest": "true",
    })
    if (published["draft"] or not published.get("immutable")
            or published["tag_name"] != "v" + version or published["target_commitish"] != sha):
        raise ValueError("Publication identity or immutability was not confirmed; inspect the release before retrying")
    return published["html_url"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["prepare", "receipt", "publish"])
    args = parser.parse_args()
    sha = workflow_commit()
    version, notes = metadata(ROOT)
    dist = ROOT / "dist"
    api = GitHub()
    if args.command == "prepare":
        validate_remote(api, version, sha)
        dist.mkdir(exist_ok=True)
        (dist / "release-notes.md").write_text(notes)
        print(f"Validated v{version} at {sha}")
    elif args.command == "receipt":
        write_receipt(dist, version, sha)
    else:
        print(publish(api, dist, version, sha, notes))


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        sys.exit(f"Release failed: {error}")
