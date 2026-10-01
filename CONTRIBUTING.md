# Contributing to Nexus

Thank you for your interest in contributing to Nexus! This document provides guidelines and information for contributors.

## Development Setup

### Prerequisites

- Rust 1.88+ (MSRV - required for edition 2024)
- Claude Code CLI (optional - SDK can auto-download)

### Building

```bash
git clone https://github.com/this-rs/nexus.git
cd nexus
cargo build
```

### Running Tests

```bash
# Run all tests
cargo test --all-features

# Run specific package tests
cargo test -p nexus-claude
cargo test -p claude-code-api

# Run with coverage
cargo llvm-cov --all-features
```

### Code Quality

```bash
# Format code
cargo fmt --all

# Run lints
cargo clippy --all-targets --all-features -- -D warnings

# Check documentation
cargo doc --all-features --no-deps
```

## Git Workflow

### Branch Structure

- `main` - Stable release branch
- `v0.x` - Version branches for ongoing development
- `feature/*` - Feature branches

For work that follows the living-documentation convention, name the branch
after what it changes, so the diagram and the code travel together:

- `docs/<domain>` - charter, index or diagram changes
- `fix/<domain>-<subject>` - a bug fix with its regression test
- `test/<domain>-<subject>` - tests only, raising coverage of a module

### Pushing

Always push with an explicit refspec:

```bash
git push origin HEAD:refs/heads/<branch>
```

Pushing `HEAD` without a refspec can create or update a branch you did not
mean to, depending on the local `push.default`. Never push directly to `main`.

### One build directory per worktree

If you use `git worktree` to work on several branches at once, give each one
its own `CARGO_TARGET_DIR`. Two worktrees sharing a target directory will
serialise on the build lock and rebuild each other's artifacts continuously:

```bash
git worktree add ../nexus-fix-transport fix/transport-version
cd ../nexus-fix-transport
export CARGO_TARGET_DIR="$PWD/target"   # not the default shared one
cargo test --all-features
```

Note that `Cargo.lock` is not tracked in this repository, so a fresh worktree
resolves dependencies from scratch. A new release of any transitive dependency
can therefore change a build that your code did not touch.

### Commit Messages

We follow [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<scope>): <description>

[optional body]

[optional footer(s)]
```

Types:
- `feat` - New feature
- `fix` - Bug fix
- `docs` - Documentation changes
- `perf` - Performance improvements
- `refactor` - Code refactoring
- `test` - Test additions/changes
- `chore` - Maintenance tasks

Examples:
```
feat(memory): add persistent conversation storage
fix(sdk): handle CLI timeout gracefully
docs: update installation instructions
```

### Pull Request Process

1. Fork the repository
2. Create a feature branch from `main` or the appropriate version branch
3. Make your changes with appropriate tests
4. Ensure all CI checks pass
5. Submit a PR with a clear description

## CI/CD Pipeline

### GitHub Actions Workflows

- **CI** (`ci.yml`) - Runs on all PRs and pushes
  - Format checking
  - Clippy lints
  - Tests (multi-platform, multi-toolchain)
  - Documentation build
  - Security audit
  - MSRV verification

- **Release** (`release.yml`) - Runs on version tags
  - Creates GitHub release
  - Builds multi-platform binaries
  - Publishes to crates.io

### Required Secrets

For maintainers setting up the repository:

| Secret | Description |
|--------|-------------|
| `CARGO_REGISTRY_TOKEN` | crates.io API token for publishing |
| `CODECOV_TOKEN` | Codecov.io token for coverage uploads |

## Release Process

1. Update version in `Cargo.toml` files
2. Create a PR to merge into version branch (e.g., `v0.5`)
3. After merge, create a version tag: `git tag v0.5.0`
4. Push the tag: `git push origin v0.5.0`
5. The release workflow will automatically:
   - Create a GitHub release with changelog
   - Build and upload binaries
   - Publish to crates.io

### Version Numbering

We follow [Semantic Versioning](https://semver.org/):
- MAJOR: Breaking API changes
- MINOR: New features, backward compatible
- PATCH: Bug fixes, backward compatible

Pre-release versions: `v0.5.0-alpha.1`, `v0.5.0-beta.1`, `v0.5.0-rc.1`

## Code of Conduct

Be respectful and constructive. We're all here to build something great together.

## Questions?

- [Open an issue](https://github.com/this-rs/nexus/issues)
- [Start a discussion](https://github.com/this-rs/nexus/discussions)

---

Thank you for contributing!
