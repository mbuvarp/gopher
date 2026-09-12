"""Exercise Cargo invalidation with the real build script in an isolated checkout."""

from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


class BuildMetadataTests(unittest.TestCase):
    def test_new_untracked_file_refreshes_cached_build_metadata(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            repo = root / "repo"
            repo.mkdir()
            shutil.copy2(Path(__file__).resolve().parent.parent / "build.rs", repo / "build.rs")
            (repo / "Cargo.toml").write_text(
                '[package]\nname = "metadata-fixture"\nversion = "0.1.0"\nedition = "2024"\n'
            )
            (repo / "src").mkdir()
            (repo / "src/main.rs").write_text(
                'fn main() { println!("{} {}", env!("GOPHER_BUILD_REVISION"), '
                'env!("GOPHER_BUILD_STATUS")); }\n'
            )
            (repo / ".gitignore").write_text("ignored/\n")

            def run(*args):
                return subprocess.run(args, cwd=repo, check=True, text=True,
                                      stdout=subprocess.PIPE, stderr=subprocess.PIPE).stdout.strip()

            run("git", "init")
            run("cargo", "generate-lockfile", "--offline")
            run("git", "add", ".")
            run("git", "-c", "user.name=Test", "-c", "user.email=test@example.invalid",
                "-c", "commit.gpgsign=false", "commit", "-m", "fixture")
            revision = run("git", "rev-parse", "--short=12", "HEAD")

            def build():
                return run("cargo", "run", "--quiet", "--offline", "--locked",
                           "--target-dir", str(root / "target"))

            # Warm Cargo and Git index stat caches before changing only untracked files.
            for _ in range(3):
                self.assertEqual(build(), f"{revision} clean")
            # No tracked file or Git metadata is changed between these builds.
            untracked = repo / "new-directory" / "new-file.txt"
            untracked.parent.mkdir()
            untracked.write_text("new untracked content")
            self.assertEqual(build(), f"{revision} uncommitted changes")
            untracked.unlink()
            untracked.parent.rmdir()
            self.assertEqual(build(), f"{revision} clean")
            (repo / "ignored").mkdir()
            (repo / "ignored/file").write_text("ignored content")
            self.assertEqual(build(), f"{revision} clean")
            run("git", "-c", "user.name=Test", "-c", "user.email=test@example.invalid",
                "-c", "commit.gpgsign=false", "commit", "--allow-empty", "-m", "new revision")
            revision = run("git", "rev-parse", "--short=12", "HEAD")
            self.assertEqual(build(), f"{revision} clean")


if __name__ == "__main__":
    unittest.main()
