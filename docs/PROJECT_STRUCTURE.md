# Project layout and maintenance

| Location | Purpose |
|---|---|
| `app/` | Shared application modules: release version/update safety, Jira access, learner metrics/evaluation and private storage |
| `installer/` | Windows installer definition, lifecycle helper, locked runtime dependencies, builder and real EXE smoke tests |
| `tests/` | Isolated regression suites, including arbitrary service-desk names, configuration safety and learner shutdown |
| `tools/` | Repeatable developer checks and safe generated-cache cleanup |
| `docs/` | Installation, migration, uninstall and developer documentation |
| `.github/` | Tested build/release workflows and shared Windows installer action |
| `dist/` | Generated public release artifacts; ignored by Git |
| Root entry points | Established launch/setup/update/evaluation commands, retained for existing users and Windows startup compatibility |
| Root JSON/JSONL/logs and `triage_backups/` | **Private live user data**, ignored by Git; not unused source or release assets |

Application entry points deliberately retain their existing names and data paths.
Renaming these would break registered Windows startup commands and could silently
start a new learner instead of using an existing model. Shared code is grouped
in `app/`; `source_layout.py` handles both this source layout and flat legacy ZIPs.
The public ZIP remains flat for compatibility with previously shipped updaters.
The installer copies only this explicit release allowlist, plus a private runtime
and its lifecycle helper. It never archives a developer's working folder.

## Developer commands

```powershell
python -m pip install -r requirements.txt
python -m pip install ruff==0.15.20
python tools/maintain.py --check
python tools/maintain.py --clean
```

`--check` runs lint, every isolated regression suite and a verified source-package
build. `--clean` deletes only Python/lint/type-checker generated caches within
known code folders; it never prunes data, reviews, backup history or releases.
It is safe to regenerate these caches. Legacy bootstrap/update commands are
supported compatibility entry points, not dead files.

The Windows CI builds with full CPython 3.14 x64, installs a SHA-256-pinned Inno
Setup compiler, and gets runtime dependencies from `installer/requirements.lock`.
It copies CPython's own standard-library inventory (not local site-packages),
then freshly installs locked wheels into the staged runtime. A `._pth` file
isolates it from `PYTHONPATH`, Python registry paths and other users' packages.
Tk, TLS, timezone data and all runtime imports are tested before packaging.

Native installer smoke tests use a clean Windows CI account: fresh install,
old-ZIP migration, upgrade/repair, data-retaining uninstall and reinstall. They
refuse to run if a real companion or native installation already exists.

`installer/lifecycle_helper.cs` is compiled by `installer/build_helper.py` using
the installed .NET Framework compiler on the build machine. There is no client
runtime compilation or PowerShell installer helper. Approved signing must happen
before helper bundling and after Setup compilation; public release verifies both
signatures against the configured publisher. See `SECURITY.md` for signing and
incident-hold requirements. Private security evidence under `.security-reports/`
is ignored and excluded from release packages.

## Scope of optimisations

The offline installer eliminates runtime downloads, pip operations and Python
discovery on users' machines. Build dependencies are isolated; test subprocesses
cannot modify live settings. Generated caches, test packages and developer
site-packages are excluded from the private runtime. Native updates refresh the
runtime as well as code; ZIP updates remain transactional and code-only.

Keep learner accuracy improvements behind chronological evaluation and adoption
gates. Installation cleanup must never remove examples, history or reviews to
make the folder appear tidier, or add a particular desk's routes/IDs to defaults.
