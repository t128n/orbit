# Orbit-CX

Orbit-CX adds enterprise GitHub features to [Orbit](https://github.com/coollabsio/orbit) as committed patch sets instead of edits to upstream source. The fork stays mergeable with upstream, and every custom change lives in `orbit-cx/patches/` where you can review, reorder, or drop it.

The patch series changes three things:

- **GitHub host**: run against GitHub Enterprise Server or Enterprise Cloud through `ORBIT_GITHUB_URL`
- **Sync mode**: keep workspaces in sync from behind a firewall, without inbound webhooks
- **Versioning**: accept SemVer build metadata in the version scripts

Each patch contains one complete feature, with its tests and follow-up fixes included. A feature that needed several pull requests upstream lands as a single patch here.

## Repository layout

The tree below shows where the patches, the helper scripts, and this documentation live:

```text
orbit-cx/
├── README.md
├── docs/
│   ├── github.md           # GitHub hosts and polling sync
│   ├── operations.md       # Environment variables, migrations, host binding
│   └── patches.md          # Patch naming, verification, rebase workflow
├── patches/
│   ├── 0001-feat-configurable-github-host.patch
│   ├── 0002-feat-github-polling-sync.patch
│   └── 0003-chore-fork-versioning-scheme.patch
└── scripts/
    ├── apply-patches.sh    # Bash: Linux, macOS, Git Bash
    └── apply-patches.ps1   # PowerShell: Windows
```

## Patch inventory

Apply the patches in filename order:

| Patch | Feature | What it changes |
|---|---|---|
| [`0001-feat-configurable-github-host.patch`](patches/0001-feat-configurable-github-host.patch) | Configurable GitHub host | Select GitHub Enterprise Server or Enterprise Cloud with `ORBIT_GITHUB_URL` |
| [`0002-feat-github-polling-sync.patch`](patches/0002-feat-github-polling-sync.patch) | Polling sync | Sync workspaces by polling with App installation tokens, including the web and mobile settings UI, HTTP registration, and the polling App manifest |
| [`0003-chore-fork-versioning-scheme.patch`](patches/0003-chore-fork-versioning-scheme.patch) | Fork versioning | Accept SemVer build metadata (`+`) in the version scripts and build tools |

## Apply the patches

You need a clean working tree, `git`, and either Bash or PowerShell. The scripts run from the repository root and read `orbit-cx/patches/` in filename order.

### Check that every patch applies

`--check` tests the whole series against a copy of the repository index and leaves your working files untouched. Run it before you apply or distribute the patches:

```bash
bash orbit-cx/scripts/apply-patches.sh --check
```

On Windows:

```powershell
powershell -ExecutionPolicy Bypass `
  -File orbit-cx/scripts/apply-patches.ps1 -Check
```

### Apply the patches to the working tree

This writes all patches into your working tree without creating commits:

```bash
bash orbit-cx/scripts/apply-patches.sh
```

### Apply the patches as commits

`--am` applies the series with `git am`, so each patch keeps its author, commit date, and commit message:

```bash
bash orbit-cx/scripts/apply-patches.sh --am
```

### Reverse the applied patches

`--reverse` rolls the series back in reverse order:

```bash
bash orbit-cx/scripts/apply-patches.sh --reverse
```

## Documentation

- [Configure GitHub Enterprise hosts and polling sync](docs/github.md)
- [Operate an Orbit-CX instance](docs/operations.md)
- [Manage and rebase patches](docs/patches.md)
