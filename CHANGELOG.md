# Changelog

Notable changes, in the format of [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versions are dates (`YYYY.MM.DD`) — see `APP_VERSION` in either app, and
Settings, for what a running install is actually on.

## [2026.08.24]

### Added
- Route-level recall, precision, confident precision, and macro-F1 reporting.
- A specialist handoff classifier that is deployed only when chronological
  validation proves it improves on the incumbent model.
- Automatic human-review guards for dormant routes and routes whose recent
  confident precision is below 95%.

### Changed
- Nightly tuning now validates across three recent windows, uses a separate
  champion/challenger gate, and keeps the newest window untouched for the
  final report.
- Tuning ranks candidates by macro F1 and precision as well as recall, so a
  rare route cannot improve its headline score by creating false positives.
- Safety policy changes are applied immediately to suggestions already shown
  in the overlay without rewriting their original scoring record.

### Fixed
- Status, suggestion, and report writes now tolerate transient Windows file
  locks without leaving partial JSON.
- Legacy IPS outcome wording is normalised in confusion reporting.
- Schema migration can save state safely before the learner is constructed.

## [2026.08.21]

### Fixed
- Three more places where one bad ticket among many could silently stall
  progress for everything after it, in `predict_intake` (the fast, ~12s
  loop), `resolve_and_learn`, and `ensure_workflow_known`'s one-time
  startup scan — the same class of bug fixed for `backfill()` in
  2026.08.19, now closed everywhere a per-issue loop had no guard around
  it. `predict_intake` in particular sees customer-submitted requests
  directly, not just hand-edited config.
- JQL injection in `triage_eval.py`'s own query building — a copy of the
  same query shape used in `triage_learner.pyw`, minus the escaping.
- The offline evaluator's fallback Python installer now runs only after
  its Authenticode signature checks out; `setup.bat` now launches
  PowerShell by fully-qualified path instead of trusting `PATH`, closing
  the last easy instance of the PATH-hijacking class fixed elsewhere in
  2026.08.19 (`launch.bat`'s `pythonw` call is the one place this can't
  be done safely — Python's own install location varies too much to pin).
- A Settings-panel hover state that was configured but invisible:
  every checkbox and radio button set its "active" colour to the same
  colour as its resting state.
- Two icons (a key, a lightbulb) that couldn't take the colour their own
  label was built around, being fixed-colour emoji rather than glyphs
  drawn in the app's own font.

## [2026.08.19]

### Added
- The learner explains its confident suggestions: the words in a ticket
  that most favoured the winning outcome, and who raised it when their
  own history backs it up.
- Nightly self-tune drift detection, calibrated against real per-class
  sample sizes so it doesn't fire on small-bucket noise.
- Model provenance, a health/refusal banner, and a machine-readable
  `triage_status.json` in the learner's report.
- A **Triage learner** section in Settings — health, provenance, drift —
  so a problem there is visible without opening a report file.
- A version identifier for both apps (`APP_VERSION`), shown in Settings
  and the report.
- `LICENSE`, `SECURITY.md`, `pyproject.toml` (ruff config), and a real
  `requirements.txt`.

### Changed
- The API token is no longer stored as plain text — encrypted at rest
  with Windows DPAPI, tied to the Windows login that saved it.
- A UI consistency pass: rounded corners, colours, and window titles
  brought in line across every window, not just the main one; a
  systematic 4px misalignment between section headings and their own
  content, present throughout Settings, fixed at the source.

### Fixed
- Several places where Jira data with an explicit `null` (present, not
  missing) instead of an omitted field reached a loop or a `len()` call
  unguarded — including one that could have stopped the learner from
  starting at all on any restart, from a single malformed history entry
  among hundreds.
- JQL injection: an unvalidated project key or an unescaped status name
  reaching a query, closed with an identifier allowlist and consistent
  literal-quoting.
- PATH-based binary hijacking: Notepad and PowerShell are now launched by
  fully-qualified `System32` path, not a bare name.
- A TOCTOU hole in both apps' atomic file writes — a predictable temp
  name that could be pre-planted and written through rather than replaced.
- The test suite no longer launches a real Notepad window (it mocks the
  process call instead), and no longer force-kills every `notepad.exe` on
  the machine as cleanup.

## [2026.08.18]

First dated version. Baseline for everything above.
