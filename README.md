# Jira Overlay

A lightweight Windows desktop companion for **Jira Service Management**. It sits quietly in the corner of your screen, pops up when new tickets arrive, and gets out of the way when there's nothing to act on.

It comes in two parts, installed and started together:

- **the overlay** — live queue counts, alerts and SLA, in the corner of your screen
- **the triage learner** — watches how your team triages tickets and predicts where each new one is heading. Strictly read-only: it only ever suggests.

Neither assumes anything about your Jira. Both work out your service desk, queues and workflow for themselves.

---

## What it does

- **Watches your JSM queues** and shows live ticket counts for each one
- **Pops up automatically** when a ticket lands in any of your chosen alert queues
- **Fires a Windows toast notification** (and optional sound) the moment a new ticket arrives — one notification per ticket, not one per refresh
- **Shows SLA compliance %** and a completed-today counter alongside each refresh
- **Hides itself** when there's nothing to action, and fades back in when there is
- Sits in the **system tray** with a live count badge so you always have a passive indicator

---

## Features

| Feature | Detail |
|---|---|
| Live queue counts | Queue counts poll every **5 s** (always on). SLA & completed-today refresh every 1 min by default (configurable: 30 s → 5 min) |
| Multiple alert queues | Tick as many as you like — at first run or in Settings; counts are summed and any of them can trigger the overlay, even if hidden from display |
| Per-ticket deduplication | Each ticket key only fires one notification, no matter how many refreshes it sits there |
| Dismiss | The hide button dismisses the overlay until a **new** ticket arrives — it won't nag you about tickets you've already seen |
| Snooze | 15 min / 30 min / 1 hour — accessible via the moon button |
| Rate-limit aware | Honours `Retry-After` on HTTP 429 and treats 5xx as transient — your data never vanishes over a server hiccup |
| Completed today | Count of tickets closed today with SLA compliance % |
| Corner snapping | Drag near a corner to snap — on **any monitor**, respecting the taskbar. Double-click the header to snap to nearest corner |
| Inline settings | Click the gear in the overlay header — expands with a smooth fade, no separate window |
| System tray icon | Badge shows the count for whichever queue you choose; right-click for quick actions |
| Always-visible mode | Optional dashboard mode that keeps the overlay on screen permanently |
| Multi-monitor aware | Clamps position to whichever monitor it's on; detects if it's been moved off-screen |
| Auto-installs dependencies | First run installs `requests`, `Pillow`, `pystray`, and `tzdata` automatically |
| Windows startup | Optional — toggle in Settings |
| Fully configurable | Every filter, threshold, SLA field name, and queue is configurable per-user |
| Single instance | A second launch quietly exits instead of double-polling and double-notifying |
| Team report | Ranks whoever you choose by tickets completed, over any date range, exportable to CSV |
| Optional triage learner | A read-only companion that predicts where each new ticket is heading, and explains why (see below) |
| Encrypted credential storage | Your API token is protected at rest with Windows DPAPI, not saved as plain text (see [Security](#security)) |

---

## Requirements

- **Windows 10 or 11**
- A **Jira Service Management** account

That's all. The recommended installer includes its own Python runtime and dependencies.

---

## Install

Download **JiraOverlay-2026.10.20.2-Setup.exe** (or the newer `-Setup.exe`) from [Releases](https://github.com/p3n1cillin/Jira-Overlay/releases/latest) and double-click it. **The same installer installs fresh, repairs, or upgrades.** No ZIP extraction, Python installation or administrator account is needed. First launch asks for that user's own Jira details.

Older ZIP/clone users can select their previous application folder during setup to copy their settings and learner data. The original folder is retained. New users leave that field blank. Never share credentials or trained models between users.

Uninstall through **Windows Settings > Apps > Installed apps > Jira Overlay** or the Start menu's **Uninstall Jira Overlay** shortcut; both use one uninstaller. Personal settings and learner data are retained for reinstallation. The EXE is currently unsigned, so Windows or corporate application-control policies may show a publisher warning.

See [installation, migration and uninstall](docs/INSTALLATION.md) for details, silent deployment and data retention. See [project structure and maintenance](docs/PROJECT_STRUCTURE.md) for the organised source layout and developer commands.

### Source/developer alternative

Developers can clone [the repository](https://github.com/p3n1cillin/Jira-Overlay), or extract the source ZIP into a stable local folder. Existing source launch commands remain supported.

Double-click **`setup.bat`** for a source install.

It does the lot:

1. Finds Python, and **installs it** if there isn't one — via winget, or straight from python.org. Per-user, so no admin prompt.
2. Installs the packages the app needs
3. Offers to start it with Windows
4. Launches it

The overlay then asks for your Jira details and finds your service desk. Nothing else to configure.

> Setup runs in a console window on purpose. The app itself runs under `pythonw`, which has nowhere to print an error — so anything that can go wrong is made to go wrong during setup, where you can read it.

Afterwards, start it with **`launch.bat`** — or just let it start with Windows.

### Application updates

Installations check the public GitHub repository on startup and every six hours. A new stable release produces a desktop notice (when desktop notifications are enabled). Settings shows the installed bundle version, **Check for updates** and **Update and restart**; the tray also has a manual check. Automatic checks can be turned off in Settings. Checks send no Jira credentials, configuration, ticket text or learner data. They are notices only: updates always require your confirmation. Native installations use the same verified Windows Setup EXE for upgrades, including their private runtime; rerunning the latest installer also repairs an install.

For legacy ZIP installations, the code-only updater downloads the complete release package, checks GitHub's SHA-256 digest and its internal file manifest, prepares dependencies, then asks both processes to save and close. Only verified application code, bootstrap files and documentation are replaced. `jira_config.json`, models, history, logs, reviews, reports and other personal files are not part of a release. The previous code is backed up under `.app-updates/backups`; a copy failure automatically restores it, and an interrupted transaction is recovered at the next launch. To explicitly roll back ZIP application code, run `update.bat --rollback` and select your installed folder. Native installations should use the installer for repair, not this code-only rollback. Model/data formats must remain backward compatible within this update channel; rolling back code does not rewind learning data.

Updates trust HTTPS and the configured GitHub publisher. Checksums protect package integrity; they are not an independent publisher signature. An unreachable GitHub API, missing package/digest, partial download or verification failure leaves the installed code alone. Never distribute your working folder, credentials or learner files to another user. Each user runs setup with their own Jira account and token.

**One-time upgrade for users of the old repository or an older ZIP:** run the latest Setup EXE and select your old folder on the migration page. Close both old tray icons first if that version lacks graceful shutdown. It preserves that user's configuration and learning data. Future releases can be installed from Settings or by rerunning Setup. The older `update.bat` source-only migration remains supported. Do not replace the whole existing folder or copy someone else's configuration into it.

**Git clones:** receive update notices but the updater refuses to overwrite a checkout. Close both apps, commit/stash any code edits as appropriate, and use `git pull --ff-only`, then run `setup.bat` if dependencies changed. When migrating from the old repository, change the checkout's remote to `https://github.com/p3n1cillin/Jira-Overlay.git` first. The project history is preserved.

### Publishing a release

1. Update `VERSION` in `app/app_release.py` and add the matching section in `CHANGELOG.md`. Both desktop processes use this single version. Use `YYYY.MM.DD` or `YYYY.MM.DD.N` for same-day releases.
2. Run `python tests/run_all.py`, `python -m ruff check .` and `python release_tools.py --output dist`.
3. Commit and push code only, then create and push the matching tag, e.g. `git tag v2026.10.20.1` and `git push origin v2026.10.20.1`.
4. The release workflow tests Windows/Python 3.9 and 3.14 again, builds the allowlisted ZIP and self-contained Setup EXE, exercises real install/upgrade/uninstall/reinstall on clean Windows, and only then publishes both packages with checksums and release notes. Users receive their next check's notice; they can also subscribe to GitHub release notifications.

The workflow can also be dispatched for an existing version tag. Never move an already-published tag or replace its assets: issue a new version instead. Fork maintainers should change `REPOSITORY` in `app/app_release.py` before publishing their own update channel. Release publishing includes only the explicit `RELEASE_FILES` allowlist (plus the native installer support script and isolated runtime), never arbitrary workspace contents.

| File | Purpose |
|---|---|
| `setup.bat` | First-time setup. Run once. |
| `launch.bat` | Starts the app. |

---

## First-run setup

After `setup.bat` finishes, the app starts and walks you through the rest:

1. **Credentials** — a setup dialog asks for:
   - **Jira Domain** e.g. `yourcompany.atlassian.net`
   - **Email** your Atlassian account email
   - **API Token** click the 🔑 link in the dialog to open the token page directly

   Paste the address however you like — a full URL such as `https://yourcompany.atlassian.net/browse/SD-1` is reduced to just the host. The details are checked against Jira before anything is saved, so a wrong token is caught there and then rather than showing up later as a silent failure.

2. **Service desk** — if your account has access to multiple service desks, a picker appears. Click the one you want to monitor.

3. **Alert queues** — a checklist of all your queues appears. Tick every queue that should trigger popups and notifications (you can change these any time in Settings), or choose to run as a passive dashboard with no alerts.

Everything is saved to `jira_config.json` next to the app — your token encrypted, never in plain text (see [Configuration file](#configuration-file)) — and the whole file is gitignored, never committed.

The triage learner needs nothing further — it works out your workflow from your own Jira the first time it runs.

---

## Usage

Once running, the overlay lives in your **system tray**. It only appears on screen when there's something to act on.

### Overlay controls

| Element | Action |
|---|---|
| **● YOUR DESK NAME** (header) | Click → opens the service desk in your browser |
| **Queue row** | Click → opens that queue in Jira |
| **✓ Completed today — N** | Click → opens today's resolved tickets in Jira with the matching JQL |
| **Refresh** (circular arrow) | Refresh now |
| **Snooze** (moon) | Snooze picker (15 min / 30 min / 1 hour). Click again while snoozed to cancel |
| **Hide** (dash) | Dismiss — hide until a **new** ticket arrives |
| **Close** (cross) | Quit — muted until hovered, so it can't be hit by accident |
| **Settings** (gear) | Open inline settings (expands overlay with a fade animation) |
| Drag | Move the overlay anywhere on screen |
| Double-click header | Snap to nearest corner |

### Tray icon

Click the tray icon to show/hide the overlay (a tray-shown overlay stays up until you hide it). Right-click for Show/Hide, **Team report** and Quit.

The badge count shows whichever queue you designate in Settings → *Tray badge shows*.

---

## Team report

Right-click the tray icon → **Team report**.

Ranks the people you choose by how many tickets they completed over a period you choose.

| Control | What it does |
|---|---|
| **Period** | Today, 7 / 30 / 90 days, or **Custom** with your own from/to dates |
| **Count by** | **Resolved date** is when the ticket was actually finished. **Last updated** is what the overlay's *Completed today* figure uses, so the two reconcile — useful if your workflow doesn't set a resolution |
| **People…** | Pick from the users Jira says can be assigned work in your project |
| **Export CSV** | Saves the current view next to the app and opens it |

Each person is one cheap count query, so it stays fast with a large team, and it runs off the UI thread so the window never freezes.

If you already have an `assignee in (...)` filter set for *Completed today*, the report seeds its list of people from it the first time you open it.

The status line also shows what share of the project's completed work the ranking accounts for, e.g. *"583 of 1028 completed (56%) · 436 had no assignee"*. That number is scoped to the project you're monitoring: if your workflow clones a ticket out to another team's project rather than assigning the original, the assignee — and the credit for finishing it — lives on that clone, in a project this report was never asked about. A low share there isn't a sign of anything wrong; it's a property of how your desk hands work off.

---

## Settings

Click the **gear** in the overlay header to open Settings inline. All changes take effect on Save.

| Setting | Description |
|---|---|
| **Stats refresh interval** | How often SLA % and Completed Today update (30 s – 5 min). Queue counts are always live at 5 s. |
| **Desktop notification** | Toast when a new ticket arrives in an alert queue |
| **Sound alert** | System beep alongside the notification |
| **Transparency** | Overlay alpha (0.3 – 1.0) |
| **Width** | Overlay width in pixels (200 – 420) |
| **Always show** | Dashboard mode — overlay stays visible even when no alert-queue tickets |
| **Hide queues whose name contains** | Comma-separated keywords — matching queues are hidden entirely |
| **Hide queues with more than N tickets** | Useful for suppressing huge archive queues (0 = no limit) |
| **Queues to show** | Per-queue visibility toggles |
| **Tray badge shows** | Which queue's count appears on the tray icon badge |
| **Alert queues** | One or more queues that trigger the overlay + notifications |
| **Show SLA compliance** | Off by choice is fine — plenty of teams have SLAs and don't track them. Turned off, the overlay stops asking Jira about them altogether |
| **SLA field name** | Which Jira field to measure against (default: `Time to resolution`). Only used when the above is on |
| **Completed today filter** | Extra JQL appended to the completed-today query e.g. `assignee is not EMPTY` |
| **Run on Windows startup** | Adds to `HKCU\...\Run` registry key. Self-heals if the install is later moved or Python is upgraded, without re-asking |
| **Reconfigure credentials** | Wipes the saved config and restarts setup — for a new domain, a rotated token, or a fresh start |

If the triage learner is installed, Settings also shows a **Triage learner** section: a green/red status dot, decisions learned, last self-tune date, accuracy, and a warning if the nightly tune drifts — all read from the learner's own status file, so a problem there (a dead token, a broken tune) is visible without opening `TRIAGE_REPORT.md`.

---

## Configuration file

`jira_config.json` is created automatically and stores all settings. You can copy `jira_config.example.json` as a starting point:

```json
{
  "domain": "yourcompany.atlassian.net",
  "email": "you@yourcompany.com",
  "token": "YOUR_API_TOKEN_HERE"
}
```

All other keys are populated automatically once the overlay has run. The file is gitignored — don't commit it.

Your API token is never stored as plain text. The first time the overlay runs (or the moment it next loads an older config), it's encrypted with Windows DPAPI and rewritten as `tokenProtected` — a blob only your own Windows login can decrypt, tied to your account rather than a password you manage. If you copy `jira_config.json` to another machine or another Windows account, the token comes across unreadable; Settings → **Reconfigure credentials** gets you a fresh one in seconds.

---

## The triage learner

The learner starts with the overlay and sits alongside it, watches how your team triages tickets, and predicts where each **new** ticket is heading. It is **strictly read-only** — the only Jira calls it makes are `GET` and search. It never transitions, comments on, edits, clones or assigns anything. Predictions are suggestions for a human, nothing more.

It starts alongside the overlay and adds a purple tray icon; right-click → *Open report*. Quit it from there if you'd rather not run it.

### It configures itself

Nothing about your workflow is hard-coded. On first run it inspects your own Jira and works out:

| What | How it's found |
|---|---|
| **Intake project** | The service desk the overlay already discovered |
| **Outcomes** | Statuses in a *done* category that tickets actually come to **rest** in. Open statuses like *In Progress* or *On Hold* are steps along the way, so they're never outcomes |
| **Resolved vs handoff** | The highest-volume outcome is ordinary resolution; the rest are handoffs to other teams (teams commonly mark those "done" too, since they're done as far as the desk is concerned) |
| **Move targets** | Detected from the issue's own key change when a ticket is moved to another project — no configuration needed, and it finds projects you never told it about |
| **Custom fields** | Request Type is matched on its JSM schema type; extra context fields are matched by **name**, never a hard-coded `customfield_NNNNN` |
| **Request-type hints** | If a portal request type overwhelmingly leads to one outcome, that becomes a hint — learned from your data, not a list someone wrote |

Whatever it infers is written back into `jira_config.json` so you can see and correct it.

### Pinning things by hand

Every discovered value is an override. Add any of these to `jira_config.json`:

| Key | Meaning |
|---|---|
| `triageStatuses` | Statuses that mean "handed off to another team" |
| `resolvedStatuses` | Statuses that mean "we finished it ourselves" |
| `moveTargetProjects` | Projects tickets get moved into |
| `contextFields` | Extra field **names** to learn from, e.g. `["Affected services", "Organizations"]` |
| `resolvedLabel` | Label for the resolved outcome (default `Resolved in <PROJECT>`) |
| `movedLabel` | Template for move outcomes (default `Moved to {project}`) |
| `movedLabels` | Per-project label overrides, e.g. `{"BUGS": "Product bug (BUGS)"}` — useful when existing history was recorded under a different name |

### What it produces

- **`TRIAGE_REPORT.md`** — provenance, cumulative and rolling 7/30-day accuracy, prediction coverage diagnostics, per-outcome and observed request-type breakdowns, confusion pairs, and outcome keywords. Rolling counts are routing events, not unique tickets.
- **`triage_status.json`** — the same health/provenance/accuracy signal as flat JSON, for anything that wants to alert on it without parsing markdown
- **`triage_suggestions.json`** — picked up by the overlay and shown next to incoming tickets
- **`triage_history.jsonl`** — labelled decisions it re-tunes itself against nightly

All of that is built from your real ticket text, so **every learner data file is gitignored**. Only the code is shared.

Run `python triage_review.py --analyse --since YYYY-MM-DD` to save local evidence-based analyses of mistakes across all outcomes. Optional `--predicted "Your outcome name"` limits the analysis to one route. Counts come from your own observed history; no team names, portal categories, or routing rules are built in. Automated notes remain pending human review and never change training labels or tickets. Use `--case "CASE_ID" --note "Your explanation"` to record a human annotation.

Nightly refitting uses the same chronological predict-then-learn step as replay, including extra weight for mistakes, so the deployed model matches the method evaluated by tuning.

The tuner fits each candidate on the chronological training prefix but ranks it on the most recent 1,200 scored decisions (or the available smaller window). Candidates must improve a separate chronological gate without losing more than 0.5 percentage points of overall or confident accuracy. The final tail remains reporting-only. A bounded route-balance adjustment can compensate for outcomes the model systematically overpredicts or underpredicts; it learns only from earlier pre-learning predictions and observed destinations, starts neutral on small/new desks, and is enabled only if validation supports it. These adjustments are ranking signals, not guarantees that displayed confidence is a calibrated probability.

Gradual-drift diagnostics compare the last 7 days with the preceding, non-overlapping 21 days, including confident suggestions, route precision and observed request types. Warnings require enough samples, at least a 3-point deterioration and conservative sample evidence. They prompt investigation, do not change routing rules, and never mark a healthy connection as failed.

For a compact review worksheet, run `python triage_review.py --export TRIAGE_REVIEW_QUEUE.md`. It prioritises frequent confusions and samples diverse, higher-confidence mistakes (default two examples per pair; customise with `--per-pair` and `--limit`). The worksheet is private ticket data and its default filename is gitignored. Refresh it with the same command. Record your own assessment using `--case CASE_ID --note "Your explanation" --verdict ambiguous`, `confirmed_routing`, or `label_issue`. Verdicts stay local and do not automatically correct labels or teach the learner a new rule.

### How it learns

Naive-Bayes over unigrams and bigrams, with separate reporter and request-type channels, complement log-odds scoring, and optional memory for recurring requests. Recency decay lets it follow changes in team routing. Nightly tuning searches from two starting configurations, compares the challenger against the incumbent on a later chronological gate, and reports a separate final window. The shared evaluator includes macro F1, precision, and routine-maintenance/other-ticket breakdowns. A higher training score alone cannot promote a candidate.

Confidence is a model score, not a guaranteed probability of correctness. Close calls and routes with insufficient precision evidence are flagged **review**. Suggestions name influential words and, where relevant, the reporter's routing history. Threshold experiments are evaluated separately before any change to deployed policy.

If a night's tune swings sharply from the one before, that's flagged too — but only when the swing is backed by enough examples not to be one ticket's worth of noise on a small destination.

`triage_eval.py` is an offline harness over the same model — `build` snapshots recent decisions, `tune` searches configs, `analyze` shows what it gets wrong.

---

## How it works

```
┌──────────────────────────────────────────────────────┐
│  Background thread — two-tier polling                 │
│                                                      │
│  Every 5 s (always):                                 │
│    GET /servicedeskapi/.../queue?includeCount=true    │  ← live counts
│    GET .../queue/{id}/issue                          │  ← alert queue issues
│                                                      │
│  Every N min (configurable, default 1 min):          │
│    POST /api/3/search/approximate-count              │  ← completed today
│    POST /api/3/search/approximate-count              │  ← SLA breaches
└────────────────┬─────────────────────────────────────┘
                 │ results queued to the main thread
                 ▼
┌──────────────────────────────────────────────────────┐
│  Main thread (tkinter)                               │
│                                                      │
│  • Update queue row counts                           │
│  • Dedup new ticket IDs                              │
│  • Fire toast / sound if new                         │
│  • Show/hide overlay via alpha fade                  │
│  • Update tray badge                                 │
└──────────────────────────────────────────────────────┘
```

- All API calls share a persistent `requests.Session` so TCP connections are reused between polls

- All Jira calls run on a **daemon background thread**; results are handed to the UI through a queue, because Tk may only be touched from the thread running its main loop
- HTTP 429 responses honour the server's `Retry-After`; 5xx and network errors keep the last data and show a subtle *Reconnecting…* status
- The overlay uses `overrideredirect(True)` — no title bar, no taskbar entry — with Windows 11 rounded corners applied via DWM, the system icon font (Segoe Fluent Icons) for its controls, and Segoe UI Variable for text
- Show and hide use a **single alpha-fade animator**, so two fades can never fight over the alpha channel. The settings panel instead **animates its geometry** — the window eases open and closed with a cubic ease-in-out, driven by the clock rather than a frame counter so the duration is honest on any machine
- Config writes are **atomic** (a uniquely-created temp file, then rename), and rewrite only this app's own keys — the triage learner stores what it discovers in the same file, and a blind dump would delete it. A corrupt config is quarantined to `.bad` rather than crashing the app

---

## Security

This talks to your real Jira instance and holds a credential that grants access to it, so it's treated the same way any code with that reach should be — not as a personal script.

- **Credentials at rest.** The API token is never written to disk as plain text — see [Configuration file](#configuration-file). Nothing else in the config is a secret worth encrypting: the domain is effectively public, and the email is your own.
- **No query injection.** Every value that reaches a Jira query is either escaped as a quoted literal or checked against a strict allowlist before being trusted as a bare identifier (a project key, a status name) — the same discipline SQL needs against unescaped input, applied to JQL. Config is meant to be hand-editable, so this is checked at the point a value is loaded, not assumed safe because it usually is.
- **No PATH-based binary hijacking, where it can be closed safely.** Notepad (fallback viewer), PowerShell (toast notifications), and `setup.bat`'s own PowerShell hand-off all resolve the fully-qualified `System32` path first, rather than trusting whatever answers to that name earliest on `PATH` — a real concern given `%LOCALAPPDATA%\Microsoft\WindowsApps`, which is user-writable, sits early in the default Windows PATH. `launch.bat` is the one exception: it calls plain `pythonw`, because Python's own install location genuinely varies by install method and scope, so there's no single fixed path to pin it to without risking the launch breaking on a real install.
- **No write-through on atomic saves.** Every "write safely" helper in this project creates its temp file with `O_CREAT|O_EXCL` (Python's `tempfile.mkstemp`), so it fails outright if anything already exists at that path instead of writing through it — closing the class of attack where a pre-planted file at a predictable name gets silently overwritten-into rather than replaced.
- **Read-only by construction.** The triage learner's Jira client exposes `GET` and search only. There is no code path from it to a mutating Jira call.
- **Signed installer only.** `setup.ps1`'s fallback path (used when `winget` isn't available) downloads the Python installer over HTTPS, then requires a valid Authenticode signature before running it — a compromised download shouldn't get further than that check.
- **Nothing phones home.** Every network call in this project goes to your own configured Jira domain, or to Microsoft's own APIs for a Windows toast notification. Nothing else.

Found a problem with any of this? See [SECURITY.md](SECURITY.md) for how to report it privately — please don't open a public issue with exploit details.

---

## Troubleshooting

| Symptom | Fix |
|---|---|
| "HTTP 401 — check credentials" | Regenerate your API token and paste it via **Settings → Reconfigure credentials** |
| SLA % never appears | Either it's switched off in Settings, or the field name doesn't match yours — try `Time to first response`. If your plan has no SLAs at all, leave it off; the overlay detects a missing field and stops asking |
| Queues missing | Check **Settings → Hide queues whose name contains** or the max-ticket threshold |
| Overlay appears on wrong monitor | Drag it to the right monitor — it saves position automatically |
| Notifications not appearing | Check Windows Focus Assist / Do Not Disturb settings |

---

## Tests

```
python tests/run_all.py
```

No test framework needed, and the whole suite runs in a few seconds. Each suite runs in its own process, because several patch imports or Tk.

| Suite | Covers |
|---|---|
| `test_config.py` | Config writing — two processes share this file, token encryption, and the settings panel surviving a malformed learner-status file |
| `test_overlay.py` | SLA on/off, the alert-queue picker, startup registration, the event pump |
| `test_report.py` | Team report periods, the JQL it builds, ranking |
| `test_notify.py` | Toast escaping (a ticket summary is untrusted text feeding a PowerShell script), the tray badge, suggestions file handling |
| `test_learner.py` | Self-configuration against a **fictional service desk** with nothing in common with any real one, JQL-injection guards, and the tuner's search mechanics |
| `test_wiring.py` | Clicks every control in the overlay and Settings — catches bindings that point at nothing |
| `test_resilience.py` | Window placement across monitors, network retry/backoff, and malformed Jira responses |
| `test_setup.py` | First run on a machine with no dependencies, a failed install, and startup registration self-healing after a moved install |

Run locally with `python tests/run_all.py`. GitHub Actions runs the suites on Windows with Python 3.9 and 3.14 for pushes and pull requests. `test_integrity.py` checks report/status/overlay agreement, interrupted polling, archived metrics, confidence evidence, backups, and offline/live replay parity.

The suites never touch a real `jira_config.json`, a real Windows registry key, or a real Notepad window — they use isolated temp files and mocked subprocess calls throughout. That isolation is deliberate: a stub config once overwrote a live install and destroyed the credentials.

---

## Changelog

### Learner review and recovery

The learner, offline evaluator, and overlay share helper modules. Keep all
`triage_*.py` files alongside the two `.pyw` entry points when updating.

- `python triage_review.py` lists unreviewed mistakes, grouped by recurring
  confusion. To record a human explanation, use
  `python triage_review.py --case "<printed case ID>" --note "<explanation>"`.
  Notes stay in `triage_reviews.json`; they do not change Jira or training labels.
- `python triage_eval.py tune` runs the same replay and acceptance gate as the
  live learner. It uses live history by default; set `TRIAGE_DATASET` to evaluate
  a particular exported JSONL dataset instead. It does not deploy changes.
- `python triage_eval.py thresholds` selects route-specific confidence thresholds
  on an earlier gate and evaluates them on a separate final window. These are
  experiments, not automatic changes to deployed policy.
- Route guards require at least 95% observed confident precision and a one-sided
  95% Wilson lower bound of at least 90%, as well as recent route activity.
  This prevents small samples from appearing more reliable than the evidence.
- New training examples retain the ticket features captured at intake. Older
  records are identified as outcome-time captures. Replay reports include both
  provenance counts and separate routine-maintenance/other-ticket results.
- The status file reports the last completed poll. A partial search retains the
  prior checkpoint for retry; routing scans paginate without a 500-ticket cap.
  The overlay marks a heartbeat older than five minutes as stale. Individual
  missing-ticket responses do not imply a project-wide outage.

On the first state save each day, the learner creates a checksum-verified local
archive in `triage_backups/YYYY-MM-DD.zip`. It includes state, training history,
the activity log, and its metrics checkpoint when present; credentials are
excluded. Archives contain private ticket data and are ignored by Git. Archives
are retained until you remove or archive them yourself.

To recover:

1. Quit the learner using its tray menu before replacing any data files.
2. Preserve the current data files in a separate folder before restoring.
3. Verify the selected archive with
   `python triage_storage.py triage_backups/YYYY-MM-DD.zip`.
4. Extract into a separate temporary folder. Copy its state, history, activity
   log and metrics checkpoint together into the application folder. If that
   archive has no metrics checkpoint, move the existing
   `triage_log.jsonl.metrics.json` out of the application folder too.
5. Keep your existing `jira_config.json`, restart the learner, and check the
   report's provenance, totals, last completed poll and health. Restoring an
   older archive rolls back learning to that archive; overlapping Jira reads
   resume from its saved checkpoint.

Activity log rotation preserves cumulative metrics in
`triage_log.jsonl.metrics.json`. Its prefix fingerprint prevents double counting
if replacement fails or the process stops during rotation.

See [CHANGELOG.md](CHANGELOG.md).

## License

[MIT](LICENSE) — do whatever you like with it.
