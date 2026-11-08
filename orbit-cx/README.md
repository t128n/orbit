# Orbit-CX

**Orbit-CX** is an enterprise-oriented extension set for [Orbit](https://github.com/coollabsio/orbit).

Instead of modifying the upstream Orbit codebase directly with online/in-tree changes, Orbit-CX maintains custom features, enterprise integrations, and operational enhancements as clean, modular, and committed patch sets. This keeps the core repository tracking upstream Orbit cleanly with zero merge conflicts on upstream releases.

---

## Directory Structure

```
orbit-cx/
├── README.md               # Overview and usage instructions
├── docs/                   # Feature and operations documentation
│   ├── github.md           # Configurable GitHub hosts & polling sync guide
│   ├── operations.md       # Operational deployment & configuration
│   └── patches.md          # Comprehensive patch inventory & rebase guide
├── patches/                # Concrete feature patch sets
│   ├── 0001-feat-configurable-github-host.patch
│   ├── 0002-feat-github-polling-sync.patch
│   └── 0003-chore-fork-versioning-scheme.patch
└── scripts/                # Helper tools for applying and verifying patches
    ├── apply-patches.sh    # Bash script (Linux, macOS, Git Bash, CI)
    └── apply-patches.ps1   # PowerShell script (Windows)
```

---

## Feature Patch Inventory

| Patch | Feature | Description |
|---|---|---|
| [`0001-feat-configurable-github-host.patch`](patches/0001-feat-configurable-github-host.patch) | Configurable GitHub Host | Support custom GitHub Enterprise Server & Cloud origins via `ORBIT_GITHUB_URL` |
| [`0002-feat-github-polling-sync.patch`](patches/0002-feat-github-polling-sync.patch) | GitHub App Polling Sync | Outbound-only polling synchronization (RSA App JWTs, web/mobile UI, HTTP registration, clean polling manifest) |
| [`0003-chore-fork-versioning-scheme.patch`](patches/0003-chore-fork-versioning-scheme.patch) | Fork Versioning Scheme | Support SemVer build metadata (`+`) in versioning scripts and build tools |

---

## Working with Patches

### 1. Dry Run / Verify Patches Cleanly
Check that all patches apply cleanly against the current repository index without modifying working files:

```bash
# Using bash:
bash orbit-cx/scripts/apply-patches.sh --check

# Or in PowerShell:
powershell -ExecutionPolicy Bypass -File orbit-cx/scripts/apply-patches.ps1 -Check
```

### 2. Apply Patches to Working Tree
Apply all patches in order:

```bash
# Using bash:
bash orbit-cx/scripts/apply-patches.sh

# Or in PowerShell:
powershell -ExecutionPolicy Bypass -File orbit-cx/scripts/apply-patches.ps1
```

### 3. Apply as Git Commits
Apply all patches preserving author, commit date, and commit messages:

```bash
bash orbit-cx/scripts/apply-patches.sh --am
```

### 4. Reverse Applied Patches
Roll back all applied patches in reverse order:

```bash
bash orbit-cx/scripts/apply-patches.sh --reverse
```

---

## Documentation

- [GitHub Enterprise & Polling Sync Guide](docs/github.md)
- [Operational & Deployment Guide](docs/operations.md)
- [Patch Management & Rebase Guide](docs/patches.md)
