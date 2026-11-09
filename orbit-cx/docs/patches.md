# Manage and rebase patches

Orbit-CX ships its features as patch files under `orbit-cx/patches/`, not as commits on a branch. You apply them to a checkout of upstream Orbit, and you rebase them when upstream changes. This page covers the file naming, the verification step, and the conflict workflow.

## Naming convention

Each patch holds one complete feature, and the four-digit prefix sets its application order:

```text
orbit-cx/patches/0001-feat-configurable-github-host.patch
orbit-cx/patches/0002-feat-github-polling-sync.patch
orbit-cx/patches/0003-chore-fork-versioning-scheme.patch
orbit-cx/patches/0004-fix-ci-checks.patch
```

The type and slug follow Conventional Commits: `feat`, `fix`, or `chore`, then a short description. A feature whose upstream work spanned several pull requests lands as one patch, with its fixes squashed in. That keeps the series short and stops a half-applied feature from sitting between two patches.

Each file is a `git format-patch` export: it keeps the author, the commit date, the subject, and the commit message body. That is what lets `--am` replay the series as commits.

## Verify the series before you apply it

Checking runs `git apply --cached` against a copy of the repository index, so the test covers the patches in sequence and leaves your working tree and real index unchanged. Run it before you apply the patches, and again after every rebase:

```bash
bash orbit-cx/scripts/apply-patches.sh --check
```

On Windows:

```powershell
powershell -ExecutionPolicy Bypass `
  -File orbit-cx/scripts/apply-patches.ps1 -Check
```

A failure names the first patch that no longer applies. Fix that patch before you continue, because later patches expect its changes.

## Rebase onto upstream

Upstream Orbit releases arrive on `upstream/main`. The sequence below keeps the patch series as the only place your changes live:

1. Confirm the working tree is clean, so you can tell a failed patch apart from your own edits:

   ```bash
   git status
   ```

2. Fetch and merge or rebase upstream:

   ```bash
   git fetch upstream
   git merge upstream/main
   ```

3. Check the series against the new tree:

   ```bash
   bash orbit-cx/scripts/apply-patches.sh --check
   ```

4. Resolve any failing patch, then repeat step 3 until the whole series applies.

Use `git rebase upstream/main` instead of `git merge` when your fork keeps local commits outside the patch files and you want a linear history.

## Resolve a conflicted patch

With `git am`, the series stops at the failing patch and leaves conflict markers in the files. Resolve the markers, stage the files, and continue the replay:

```bash
bash orbit-cx/scripts/apply-patches.sh --am
git add -A
git am --continue
```

Export the patched commit to a file, so the author, date, and message stay intact:

```bash
git format-patch -1 HEAD --stdout \
  > orbit-cx/patches/0002-feat-github-polling-sync.patch
```

For a working-tree apply, `--reject` writes the failing hunks to `.rej` files and applies the rest. Edit the source files, stage them, commit with the subject from the patch file, then export the commit with `git format-patch -1 HEAD`. Delete the `.rej` files before you continue.

Keep the filename, or update the inventory table in the [README](../README.md) and the order in the numbering. Then run the check again to confirm the series still applies end to end.

## Watch for migration number collisions

Upstream adds migrations in `apps/server/migrations/` and `apps/server/migrations/postgres/`. A new upstream migration can take `0078` or `0079`, which this series already uses. Renumber both copies in that case, then update the schema version assertion in the platform tests. See [Operate an Orbit-CX instance](operations.md) for the migration inventory.
