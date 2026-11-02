# Changelog

Notable changes, in the format of [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versions are dates (`YYYY.MM.DD`, optionally `.N` for same-day bundles) — see `app_release.py`, and
Settings, for what a running install is actually on.

## [2026.10.22.1]

Security-review build: not released or approved for production deployment.

### Security architecture
- Retired scheduled-task registration/execution and temporary PowerShell updater helpers. User-session recovery is launched normally from Windows, with no task/service persistence.
- Background checks notify only, including when older settings enabled automatic installation. Explicit update navigation opens the configured release page; downloaded code is never executed by the app.
- Native installer lifecycle operations use a build-time compiled .NET Framework helper, cooperative IPC and ownership-validated legacy-task retirement. No script-policy override, runtime compilation, remote-memory operation or forced process termination is used. Older apps without shutdown IPC require manual Quit.
- Fresh installs leave Windows startup and initial launch unchecked. Unchanged startup settings are not repeatedly rewritten. A local security-hold file is preserved and prevents launch.
- Existing installations and retained datasets are repaired in place without selecting a previous folder or merging learners. Separate ZIP import is an explicit, unchecked option for empty destinations; no old folder is auto-selected.
- Signing is optional by project-owner decision. When configured, both the lifecycle helper and installer are signed and verified against the approved identity. Unsigned builds are identified honestly; checksums alone are not publisher authentication. Defender validation and investigation are not replaced by signing.

## [2026.10.22]

### Reliability and diagnostics
- Upgrades preserve the live Windows startup setting rather than restoring the installer's older selection. Fresh installs still offer startup by default; explicit deployment opt-outs remain respected.
- Settings shows whether automatic recovery is actually running, a companion is deliberately closed, or restart backoff/setup is pending, with one-click access to the local diagnostic logs. A saved PID alone is never treated as proof of a running guardian.
- Duplicate guardian launches cannot overwrite the active guardian's diagnostics. Recovered components clear stale exit codes, guardian readiness is recorded correctly, and failed/repeated launches release diagnostic streams without accumulating closed handles.
- Reporting regression checks locate concurrent queries by identity instead of assuming execution order, eliminating false failures without serialising reporting requests.
- Added isolated recovery/status/UI tests and native enabled/disabled-startup repair/upgrade coverage. No routing thresholds, learned models, tenant defaults or personal data formats changed.

## [2026.10.21]

### Independent launch and recovery
- Managed installations launch through a per-user, demand-only Windows task instead of remaining descendants of a developer terminal or Codex session. No password, administrator service or separate logon trigger is used; the existing Windows startup preference remains authoritative.
- A guardian recovers unexpectedly exited overlay/learner processes with bounded backoff, captures child exit codes and never force-kills a running writer. First-run credentials must exist before the learner is launched.
- Deliberate Quit is persisted separately for each companion and is not undone by recovery. Normal launching explicitly starts both again; update relaunch preserves those choices.
- Updates and uninstall request shutdown of the guardian as well as the companions, waiting for active saves and all private-runtime users before replacing/removing files. Uninstall removes its own recovery task, retaining personal data and diagnostics.
- Bounded local lifecycle/fault/stderr logs record starts, clean exits, update shutdowns and unexpected termination without serialising Jira credentials, ticket data or exception text into lifecycle records.
- Restricted Task Scheduler policy leaves normal launching available and records `JIRA-R101` rather than making the application unusable.

## [2026.10.20.5]

### Automatic updates
- Published installs automatically download and verify newer stable releases, save/close both apps, install and relaunch. Automatic installation is on by default, with a Settings opt-out; disabling checking disables automatic installation too. Manual updates retain confirmation.
- Native updates are supervised by an external Windows process, not the private Python runtime being replaced. Setup shows progress, suppresses confirmation prompts and never automatically reboots Windows.
- Durable update status and a six-hour per-version retry delay prevent repeated failed downloads/installation on every app launch. Manual retries and newer releases remain available.
- Installer failures attempt to relaunch the retained/rolled-back app; failure reasons/codes are shown once and kept in local diagnostics. ZIP updates also report helper failure without leaving controls stuck busy.
- Git and organised source trees remain excluded. Credentials, tenant-specific configuration, personal learner data, existing startup preference and older ZIP updater compatibility are preserved.
- Added supervisor/checksum/failure/relaunch/opt-out regression tests and a native CI upgrade launched from the private interpreter, which exits before its own runtime is replaced.

## [2026.10.20.4]

### Installer recovery
- Setup automatically requests shutdown for supported builds, and closes identifiable older builds through their Tk/pystray window loops. Matching checks the script path, Windows user and session; unrelated Python programs are never terminated.
- Unidentifiable running copies produce an immediate instruction to Quit both tray icons and Retry, including during a fresh install, instead of an invisible ten-minute wait.
- Supported copies receive a safe shutdown request; a bounded wait reports progress and explains how to retry if the learner is still saving. No learner is force-killed.
- Preparation has a staged progress bar, helper output is captured, and setup logs are enabled by default. Stable JIRA-Ixxx failure codes distinguish path validation, app shutdown, data migration, startup registration and helper/policy failures.
- Migration checks only the personal files being imported, avoiding unrelated old virtual environments and backup trees. Originals and credentials are preserved.
- Regression coverage includes actual running legacy Tk/pystray loops, cooperative and slow shutdown processes, automatic old-build quit, plus blocked native installation retries.

## [2026.10.20.3]

### Reliability
- One coordinated Windows startup command starts the learner only after credentials are ready. Startup detection checks the actual install/interpreter paths; enabling creates a missing Run key and retires the old independent learner command.
- Workflow/field discovery retries automatically with bounded, interruptible backoff instead of leaving an inactive tray process after a transient outage. Failed field discovery no longer persists empty defaults.
- Learner status/report surfaces discovery and polling failures immediately, then clears the warning after a completed learning poll.
- Malformed, repeated and incomplete Jira pages retain valid records but cannot advance a full-poll checkpoint. Repeated changelog pages terminate safely, and explicit historical sampling caps are distinguished from transport failures.
- Interrupted historical reads retain learning progress and retry automatically without replaying already learned transitions.
- Retry-After supports HTTP dates and fractional seconds; cancellation prevents new requests after shutdown.

### Compatibility
- No site-specific routes, tenant IDs, accuracy thresholds or model weights changed. Existing models, user configuration, private histories and legacy ZIP updater compatibility are preserved.

## [2026.10.20.2]

### Added
- One self-contained Windows Setup EXE for fresh installs, repairs and upgrades; includes a private Python/Tk runtime and locked dependencies (no Python installation or administrator account required).
- Standard Start menu shortcuts and one uninstaller in Windows Installed apps. Uninstall removes installed program files and owned startup entries, retaining credentials and learner data for reinstallation.
- Optional verified migration from old ZIP/clone folders, with automatic discovery from Windows startup entries. Originals and their historical backups are never deleted; existing personal datasets are never merged or overwritten.
- First-run launch ordering starts the learner only after the overlay has collected the fresh user's credentials, avoiding a missing-configuration error during setup.
- Native installations use the same verified installer for in-app upgrades, including runtime updates. Older ZIP installations retain their code-only updater.
- Windows installer lifecycle regression tests and automated fresh-install, upgrade, uninstall and reinstall smoke tests.

### Safety
- Per-user installation only, graceful shutdown without force-killing learners, Git checkout/junction protection, and no service-desk-specific defaults.
- The executable is currently unsigned; checksums verify downloads but are not a publisher signature.

### Project organisation
- Grouped shared application modules under `app/`, separated installer, tools, tests and documentation, and documented stable compatibility entry points/data paths.
- Added one repeatable verification command and narrowly scoped generated-cache cleanup; personal models, history and backups are never classified as dead files.
- Source layout and published flat ZIP compatibility are handled centrally, keeping already-released updater clients working.

## [2026.10.20.1]

### Added
- Public, versioned application releases from p3n1cillin/Jira-Overlay with a clean code-only ZIP and SHA-256 checksums.
- Background update notices, a manual check in Settings/tray and a confirmed update-and-restart flow for ZIP installations.
- Verified staging, graceful learner shutdown, code backups, automatic rollback on copy failure and interrupted-update recovery.
- A one-time migration helper (`update.bat`) for older installations, plus rollback support.

### Changed
- Overlay and learner now share one release version. Git checkouts receive notices but are never overwritten by the updater.
- This first bundled release also includes the September/October learner, integrity, reporting and review improvements below.

## [2026.10.20]

### Added
- Bounded, data-learned route-balance ranking candidates, neutral without sufficient evidence and subject to chronological validation.
- Gradual drift diagnostics comparing a recent week with the preceding disjoint three weeks, by route, request type and confident band.
- Prioritised, diverse mistake-review samples, private Markdown worksheets and explicit local human verdicts.
- Regression tests for route-name invariance, pre-learning feedback, accuracy safeguards, drift boundaries and human-review provenance.

### Changed
- Candidate selection scores the recent training tail rather than letting old successes dominate.
- Gate adoption also protects overall and confident accuracy from material regression.

## [2026.10.07]

### Fixed
- Nightly model refits retain mistake weighting and match chronological replay.
- Time-sensitive learner fixtures use relative dates rather than expiring.
- Historical suggestions with unrecorded review flags are not counted as confident.

### Added
- Rolling 7/30-day route performance and evidence-based coverage diagnostics.
- Automated local mistake analysis, kept separate from human annotations and labels.
- Refit-equivalence, coverage, and arbitrary service-desk naming regression tests.

### Changed
- Request-type reporting uses observed portal names rather than a site-specific maintenance bucket.
- Mistake analysis supports every outcome; offline handoff roles use discovered configuration or explicit record evidence, not English status-name prefixes.

## [2026.09.07]

### Fixed
- Route-table counters no longer overwrite the overall status accuracy.
- A missing individual ticket no longer leaves a permanent project-outage
  warning; successful searches clear prior access warnings.
- Interrupted routing searches retain their checkpoint and scans paginate
  beyond 500 tickets. The checkpoint uses scan start time.
- Cumulative route metrics survive log rotation and restart, including
  equal-size replacement files and interrupted replacement.

### Added
- Intake feature snapshots, mistake review with local annotations, separate
  routine-maintenance metrics, and sample-size-aware confidence guards.
- Route-threshold experiments with a separate final evaluation window.
- Last-successful-poll status, stale-heartbeat detection in the overlay,
  daily verified recovery archives, and recovery instructions.
- Windows CI and cross-component integrity regression coverage.

### Changed
- Extracted Jira transport, evaluation, reporting helpers, and backup storage
  into modules. Offline and live tuning now share the same acceptance gate.
- Tuning records why a challenger was accepted or rejected and the comparison
  scores. Offline evaluation defaults to current live history.

## [2026.08.26]

### Added
- Launching the overlay a second time no longer exits silently — it signals
  the running instance, which shows itself (a named Windows event handed to
  the main thread through the existing queue).
- Notifications say where a ticket landed: every alert names its queue —
  a summary-less alert becomes "New ticket in Escalations", and a summary
  carries an extra `In <queue>` line. The hover popup groups tickets under
  per-queue headings (when more than one alert queue is configured) and
  lists a ticket matching several alert queues only once.
- `triage_eval.py build` guards every per-issue pass like the learner's own
  backfill — one malformed ticket no longer aborts the whole dataset build —
  writes the dataset atomically, and cuts off on the Jira account's timezone.
- Learning windows and cutoffs in the triage learner are now built in the
  Jira account's own profile timezone (read once from `/myself`), not the
  workstation's — a machine/Jira timezone mismatch or DST difference no
  longer silently shifts every learning window. `tzdata` (the IANA database,
  absent on Windows) is installed alongside the other dependencies.
- The overlay's error log rotates at 5 MB, and the learner's activity log at
  20 MB, so neither grows unbounded.

### Changed
- Alert-queue issue pulls run four-wide instead of queue-after-queue, so a
  desk watching several queues fits comfortably inside the 5-second live
  poll. Each pull slot keeps its own connection pool for the life of the
  app — a shared `requests.Session` across threads races its cookie jar,
  and per-call sessions would re-handshake TLS every poll.
- The first-run alert-queue picker offers checkboxes for several queues
  instead of forcing a single choice — adding the second alert queue no
  longer requires a trip into Settings.
- The team report counts everyone four-wide instead of one HTTP round-trip
  per person — a ten-person desk's ranking lands in about a quarter of the
  time. Each worker keeps its own connection pool: a shared
  `requests.Session` across threads races its cookie jar.
- Error back-off carries ±15% jitter, so a fleet of overlays recovering
  from a shared outage doesn't retry in lockstep and knock the server back
  over.
- The team report reads its "Count by" setting on the main thread before the
  background count starts; reading a Tk variable on a worker thread could
  crash or deadlock the count.
- Config saves are serialised across threads — a fetch-thread save could
  previously interleave with a main-thread save and lose keys.
- Slow-loop routing searches fetch up to 500 issues per cycle instead of 50:
  after the machine was asleep for days, transitions past the first page were
  silently lost for good. Intake predictions likewise cover 500 tickets, and
  the People picker paginates past the first 200 assignable users.
- Fallback issue counting (older Jira without approximate-count) follows
  pagination instead of capping every figure at "200+".
- Opening the People picker no longer blocks the UI on the Jira lookup; the
  list fills in when it returns, keeping whatever was already ticked.
- Both apps close their HTTP sessions on shutdown.

### Fixed
- One malformed record in the learner's activity log tail could kill every
  future report permanently (the log is append-only, so the bad line never
  aged out).
- A fragile single-instance mutex check in the learner could read a stale
  last-error and let two instances run.
- An unwritable learner log file crashed error paths instead of being
  tolerated like the overlay's.
- `suggestionsFile` in the config is now treated as a bare filename — a
  hand-edited path traversal value can no longer point the overlay at
  arbitrary files to display as suggestions.

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
