# Patch Management & Rebase Guide

Orbit-CX manages custom features as discrete patch files committed under `orbit-cx/patches/`.

---

## Feature Patch Organization

Each patch encapsulates one complete, production-ready feature with all tests and fixes included:

```
orbit-cx/patches/0001-feat-configurable-github-host.patch
orbit-cx/patches/0002-feat-github-polling-sync.patch
orbit-cx/patches/0003-chore-fork-versioning-scheme.patch
```

---

## Verifying Patches

Before applying or distributing patches, verify that all patch sets apply cleanly in sequence against the current tree:

```bash
# Via bash:
bash orbit-cx/scripts/apply-patches.sh --check

# Or in PowerShell:
powershell -ExecutionPolicy Bypass -File orbit-cx/scripts/apply-patches.ps1 -Check
```

---

## Rebasing with Upstream Orbit

When pulling updates or releases from upstream Orbit (`upstream/main`):

1. **Ensure Working Tree is Clean**:
   ```bash
   git status
   ```

2. **Fetch and Merge/Rebase Upstream**:
   ```bash
   git fetch upstream
   git merge upstream/main  # or git rebase upstream/main
   ```

3. **Check Patch Compatibility**:
   ```bash
   just check-patches
   ```

4. **Resolving Conflicts (if any)**:
   If an upstream change introduces a conflict with a patch:
   - Apply patches up to the conflicting one.
   - Apply the conflicting patch with `git apply --reject <patch>`.
   - Inspect `.rej` files, resolve conflicts in source files.
   - Regenerate the patch file with `git diff <files>`.
   - Re-run `just check-patches` to verify the series.
