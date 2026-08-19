# Changelog

Notable changes, in the format of [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versions are dates (`YYYY.MM.DD`) — see `APP_VERSION` in either app, and
Settings, for what a running install is actually on.

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
- CI: the full test suite and a lint check now run on every push, on
  Windows, against both the oldest supported Python and the current one.
- `LICENSE`, `SECURITY.md`, `pyproject.toml` (ruff config), Dependabot,
  and a real `requirements.txt`.

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
