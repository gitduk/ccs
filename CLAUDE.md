# Notice

1. Before every `git commit`, bump the version in `Cargo.toml` (patch = fix/refactor, minor = new feature, major = breaking change), then include `Cargo.toml` + `Cargo.lock` in the commit.
2. After completing work, run `cargo clippy` to check for warnings and issues.

# Releases & Versioning

- Tag releases with semver `v1.2.3`; annotated tags with a changelog summary
- When code changes affect behavior, features, APIs, or bug fixes, bump the version:
  - `patch` = bug fix, `minor` = new feature, `major` = breaking change
  - Update `Cargo.toml` / `package.json` / `pyproject.toml`
- Always flag if a version bump is missing

