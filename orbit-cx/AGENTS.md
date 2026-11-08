# Orbit-CX agent instructions

`orbit-cx/` holds the fork's enterprise GitHub features as patch files, the docs that describe them, and the scripts
that apply the patches. The rest of the repository is upstream Orbit. Read [`../AGENTS.md`](../AGENTS.md) for the app,
database and dev-server rules; this file covers only the overlay.

## Two documentation trees

Both trees are Markdown, but they serve different readers. Edit the fork's copy for operator-facing changes to the
overlay. Edit the upstream copy only as part of a patch.

| Path | Owner | Edit when |
|---|---|---|
| [`../docs/github.md`](../docs/github.md), [`../docs/operations.md`](../docs/operations.md) | Upstream Orbit; the patch series adds sections | The change belongs to a patched feature, such as host configuration or polling sync |
| `docs/github.md`, `docs/operations.md`, `docs/patches.md` | This fork | Documenting the overlay itself: patch workflow, operator steps, limits |

Patch 0001 edits `docs/github.md`, `docs/operations.md` and `deploy/`; patch 0002 edits `docs/github.md`; patch 0003
edits `justfile`. No patch touches `orbit-cx/docs/`, so edit those files directly.

## Patch files are generated

Do not hand-edit `patches/*.patch`. Make the change on a branch, then export it, so the author, date and commit message
survive `--am`:

```bash
git format-patch -1 HEAD --stdout \
  > orbit-cx/patches/0002-feat-github-polling-sync.patch
```

Verify the series on a clean tree before and after any patch change:

```bash
bash orbit-cx/scripts/apply-patches.sh --check
```

Apply with the same script: no flag for the working tree, `--am` to commit, `--reverse` to roll back. The justfile has
no patch recipes, so do not document `just check-patches` or `just apply-patches`.

## Verify documentation edits

No check automates these rules yet, so verify them by hand before you commit a doc change:

- Every relative link resolves from the file that contains it. Re-check after moving or renaming a file.
- Every `NNNN-*.patch` filename in prose exists under `patches/`, and every documented script flag exists in
  `apply-patches.sh` or `apply-patches.ps1`.
- Every `just <recipe>` named in prose exists in `justfile`.
- Every environment variable, table name and migration number matches the patch hunks, not the previous prose.

## Documentation style

Match the existing fork docs: sentence-case headings, one command per code block with a language tag, no `---` rules,
no em dashes, and code lines under 80 columns. Keep each page to one job and open with a summary sentence.
