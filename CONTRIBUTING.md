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
- `docs/*`, `fix/*`, `test/*`, `chore/*` - Topic branches (see *Branch naming*)

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

### Branch naming

| Prefix | Use |
|---|---|
| `docs/<domain>` | documentation and diagrams only for a domain |
| `fix/<domain>-<subject>` | bug fix |
| `test/<domain>-<subject>` | tests only |
| `chore/<subject>` | tooling, CI, maintenance |

### One PR = one vertical slice

A pull request carries a complete slice: the fix, a regression test that
**fails without the fix** (paste the failing command and output in the PR),
the updated diagram under `docs/diagrams/<name>.mmd` (or an explicit
`Diagram-Unchanged: <name> — <reason>`), and the documentation. Diagram-only
PRs describing unverified code are not accepted. The PR template checklist
must be fully ticked; an unticked box means the PR is not mergeable.

Always report coverage as two figures when both exist: **raw** and **gated**
(see `docs/COVERAGE.md`). Never exclude code from coverage to make a number pass.

### Pushing

Never push directly to `main`. Push your branch with an explicit refspec:

```bash
git push origin HEAD:refs/heads/<branch>
```

Do not create releases, tags or version bumps in a feature PR.

### Build directory per worktree

When you use several `git worktree`s, give each one its own build directory so
they never contend for the same Cargo lock or invalidate each other's cache:

```bash
export CARGO_TARGET_DIR=/path/to/target-<worktree-name>
```

### Pre-push hooks

Make sure the Rust toolchain is on the `PATH` before pushing, otherwise the
pre-push hooks cannot find `cargo`:

```bash
PATH=$HOME/.cargo/bin:$PATH git push origin HEAD:refs/heads/<branch>
```
Never bypass the hooks with `--no-verify`.

### Patch coverage gate

`codecov.yml` has a blocking patch status: lines added or modified by a PR
must be at least 80% covered. The project-wide status stays informational until
the baseline is raised (currently about 69% for `nexus-claude`, 39% for `claude-code-api`).

### Pull Request Process

1. Fork the repository
2. Create a branch from `main` or the appropriate version branch (see *Branch naming*)
3. Make your changes with appropriate tests
4. Ensure all CI checks pass
5. Push with `git push origin HEAD:refs/heads/<branch>` and open a PR, filling in the PR template

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
