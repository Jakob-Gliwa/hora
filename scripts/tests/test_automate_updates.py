import copy
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    "updates", Path(__file__).resolve().parent.parent / "automate-updates.py")
updates = importlib.util.module_from_spec(spec)
spec.loader.exec_module(updates)


class PolicyTests(unittest.TestCase):
    def setUp(self):
        self.old = {"package": [
            {"name": "hora", "version": "0.1.0"},
            {"name": "library", "version": "0.3.10", "source": updates.REGISTRY, "checksum": "old"},
        ]}
        self.new = copy.deepcopy(self.old)
        self.new["package"][1].update(version="0.3.11", checksum="new")

    def test_stable_patch_allowed_including_zero_major(self):
        self.assertTrue(updates.compatible_update(self.old, self.new))
        self.new["package"][1]["version"] = "0.3.11+custom"
        self.assertTrue(updates.compatible_update(self.old, self.new))

    def test_semver_compatible_minor_allowed_with_build_metadata(self):
        for before, after in [("1.1.8+spec-1.1.0", "1.2.0+spec-1.1.0"), ("2.0.0", "2.1.1")]:
            with self.subTest(before=before):
                self.old["package"][1]["version"] = before
                self.new["package"][1]["version"] = after
                self.assertTrue(updates.compatible_update(self.old, self.new))

    def test_incompatible_lines_need_review(self):
        for before, after in [("0.0.3", "0.0.4"), ("1.9.0", "2.0.0"), ("1.2.0+a", "1.2.0+b")]:
            with self.subTest(before=before):
                self.old["package"][1]["version"] = before
                self.new["package"][1]["version"] = after
                self.assertFalse(updates.compatible_update(self.old, self.new))

    def test_minor_major_prerelease_and_downgrade_need_review(self):
        for value in ["0.4.0", "1.0.0", "0.3.9", "0.3.12-rc.1"]:
            with self.subTest(value=value):
                self.new["package"][1]["version"] = value
                self.assertFalse(updates.compatible_update(self.old, self.new))

    def test_source_changes_new_crates_and_local_changes_need_review(self):
        for mutate in [
            lambda d: d["package"][1].update(source="git+https://example.com/library"),
            lambda d: d["package"].append({"name": "new", "version": "1.0.0"}),
            lambda d: d["package"][0].update(version="0.1.1"),
            lambda d: d["package"].pop(0),
        ]:
            value = copy.deepcopy(self.new)
            mutate(value)
            self.assertFalse(updates.compatible_update(self.old, value))

    def test_unchanged_version_cannot_change_checksum(self):
        self.new["package"][1]["version"] = "0.3.10"
        self.assertFalse(updates.compatible_update(self.old, self.new))
        self.assertFalse(updates.compatible_update(self.old, self.old))

    def test_multiple_release_lines_and_unchanged_build_metadata(self):
        extra = [{"name": "library", "version": "1.0.0", "source": updates.REGISTRY},
                 {"name": "toml", "version": "1.1.8+spec-1.1.0", "source": updates.REGISTRY}]
        self.old["package"].extend(extra)
        self.new["package"].extend(extra)
        self.assertTrue(updates.compatible_update(self.old, self.new))

    def test_ambiguous_duplicate_versions_need_review(self):
        self.new["package"].append(copy.deepcopy(self.new["package"][1]))
        self.assertFalse(updates.compatible_update(self.old, self.new))

    def test_only_base_image_digests_may_change(self):
        image = "FROM alpine:3.24@sha256:" + "a" * 64 + " AS runtime"
        before = f"# syntax=docker/dockerfile:1\n{image}\nRUN true\n"
        digest = image.replace("a" * 64, "b" * 64)
        self.assertTrue(updates.digest_only(before, before.replace(image, digest)))
        for after in [before,
                      before.replace(image, digest.replace("3.24", "3.25")),
                      before.replace(image, digest.replace("alpine:", "evil/alpine:")),
                      before.replace(image, digest.replace(" AS runtime", " AS other")),
                      before.replace(image, digest).replace("RUN true", "RUN curl evil"),
                      before.replace(image, digest) + "RUN extra\n",
                      before.replace(image, image.replace("@sha256:" + "a" * 64, ""))]:
            with self.subTest(after=after):
                self.assertFalse(updates.digest_only(before, after))

    def test_repository_dockerfile_pins_every_base_image(self):
        dockerfile = (Path(__file__).resolve().parents[2] / "Dockerfile").read_text()
        bases = [line for line in dockerfile.splitlines()
                 if line.startswith("FROM ") and not line.startswith(("FROM scratch", "FROM build-"))]
        self.assertTrue(bases)
        for line in bases:
            with self.subTest(line=line):
                self.assertRegex(line, updates.FROM_DIGEST)

    def test_main_without_ci_run_is_dispatched_once(self):
        for runs, expected in [([], True), ([{"head_branch": "main"}], False)]:
            with self.subTest(runs=runs), \
                 patch.object(updates, "api", side_effect=[{"object": {"sha": "abc"}},
                                                           {"workflow_runs": runs}]), \
                 patch.object(updates, "dispatch") as dispatch:
                updates.ensure_main_ci()
                if expected:
                    dispatch.assert_called_once_with("ci.yml", "main")
                else:
                    dispatch.assert_not_called()

    def test_unattended_files_are_routed_to_their_policy(self):
        pr = {"number": 8, "user": {"login": "dependabot[bot]", "type": "Bot"}, "draft": False,
              "head": {"sha": "head", "repo": {"full_name": updates.REPO}}, "base": {"sha": "base"}}
        for filename, merged in [("Dockerfile", True), (".github/workflows/ci.yml", False)]:
            files = [{"filename": filename, "status": "modified"}]
            with self.subTest(filename=filename), \
                 patch.object(updates, "pages", side_effect=[[pr], files]), \
                 patch.object(updates, "digest_only", return_value=True), \
                 patch.object(updates, "content", return_value=""), \
                 patch.object(updates, "api", side_effect=[
                     {"head": {"sha": "head"}, "mergeable_state": "clean"}, {"merged": True}]) as api, \
                 patch.object(updates, "dispatch") as dispatch:
                self.assertEqual(updates.merge_updates(), merged)
                if merged:
                    api.assert_called_with("pulls/8/merge", "PUT", {"sha": "head", "merge_method": "squash"})
                    dispatch.assert_called_once_with("ci.yml", "main")
                else:
                    api.assert_not_called()

    def test_pending_checks_or_moved_head_block_the_merge(self):
        pr = {"number": 8, "user": {"login": "dependabot[bot]", "type": "Bot"}, "draft": False,
              "head": {"sha": "head", "repo": {"full_name": updates.REPO}}, "base": {"sha": "base"}}
        files = [{"filename": "Dockerfile", "status": "modified"}]
        for detail in [{"head": {"sha": "head"}, "mergeable_state": "blocked"},
                       {"head": {"sha": "moved"}, "mergeable_state": "clean"}]:
            with self.subTest(detail=detail), \
                 patch.object(updates, "pages", side_effect=[[pr], files]), \
                 patch.object(updates, "digest_only", return_value=True), \
                 patch.object(updates, "content", return_value=""), \
                 patch.object(updates, "api", return_value=detail) as api, \
                 patch.object(updates, "dispatch") as dispatch:
                self.assertFalse(updates.merge_updates())
                api.assert_called_once_with("pulls/8")
                dispatch.assert_not_called()

    def test_foreign_author_or_fork_is_never_merged(self):
        for author, repo in [("human", updates.REPO), ("dependabot[bot]", "other/fork")]:
            pr = {"user": {"login": author, "type": "Bot"}, "draft": False,
                  "head": {"repo": {"full_name": repo}}}
            with patch.object(updates, "pages", return_value=[pr]), patch.object(updates, "api") as api:
                self.assertFalse(updates.merge_updates())
                api.assert_not_called()


if __name__ == "__main__":
    unittest.main()
