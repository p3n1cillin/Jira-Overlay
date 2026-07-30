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
| Multiple alert queues | Tick as many queues as you like — counts are summed and any of them can trigger the overlay, even if hidden from display |
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
| Auto-installs dependencies | First run installs `requests`, `Pillow`, and `pystray` automatically |
| Windows startup | Optional — toggle in Settings |
| Fully configurable | Every filter, threshold, SLA field name, and queue is configurable per-user |
| Single instance | A second launch quietly exits instead of double-polling and double-notifying |
| Optional triage learner | A read-only companion that predicts where each new ticket is heading (see below) |

---

## Requirements

- **Windows 10 or 11**
- A **Jira Service Management** account

That's all. Setup installs Python and everything else if the machine hasn't got it.

---

## Install

Download the project (**Code → Download ZIP**, then extract — or `git clone`), then:

### Double-click **`setup.bat`**

It does the lot:

1. Finds Python, and **installs it** if there isn't one — via winget, or straight from python.org. Per-user, so no admin prompt.
2. Installs the three packages the app needs
3. Offers to start it with Windows
4. Launches it

The overlay then asks for your Jira details and finds your service desk. Nothing else to configure.

> Setup runs in a console window on purpose. The app itself runs under `pythonw`, which has nowhere to print an error — so anything that can go wrong is made to go wrong during setup, where you can read it.

Afterwards, start it with **`launch.bat`** — or just let it start with Windows.

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

2. **Service desk** — if your account has access to multiple service desks, a picker appears. Click the one you want to monitor.

3. **Alert queue** — a list of all your queues appears. Click one to set your initial alert queue (the one that triggers popups and notifications). You can add more alert queues later in Settings.

Everything is saved to `jira_config.json` next to the app. It holds your API token, so it is gitignored and never committed.

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

Click the tray icon to show/hide the overlay (a tray-shown overlay stays up until you hide it). Right-click for Show/Hide and Quit.

The badge count shows whichever queue you designate in Settings → *Tray badge shows*.

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
| **SLA field name** | The Jira field used for SLA compliance (default: `Time to resolution`) — leave blank to disable SLA tracking |
| **Completed today filter** | Extra JQL appended to the completed-today query e.g. `assignee is not EMPTY` |
| **Run on Windows startup** | Adds to `HKCU\...\Run` registry key |

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

All other keys are populated automatically once the overlay has run. The file holds your API token, so it is gitignored — don't commit it.

---

## Triage Shadow Learner (optional)

`triage_learner.pyw` is a companion that sits alongside the overlay, watches how your team triages tickets, and predicts where each **new** ticket is heading. It is **strictly read-only** — the only Jira calls it makes are `GET` and search. It never transitions, comments on, edits, clones or assigns anything. Predictions are suggestions for a human, nothing more.

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

- **`TRIAGE_REPORT.md`** — accuracy, per-outcome breakdown, confusion pairs, and the keywords driving each outcome
- **`triage_suggestions.json`** — picked up by the overlay and shown next to incoming tickets
- **`triage_history.jsonl`** — labelled decisions it re-tunes itself against nightly

All of that is built from your real ticket text, so **every learner data file is gitignored**. Only the code is shared.

### How it learns

Naive-Bayes over unigrams + bigrams, with separate channels for reporter and request type, complement log-odds scoring (so vocabulary common to every outcome cancels out), and a nearest-neighbour "ticket memory" for recurring requests. Recency decay lets it follow changes in how your team routes. Every night it replays its own history under a coordinate-descent search over nine hyper-parameters and adopts the best, scored on a held-out tail so improvements are real rather than fitted.

Confidence is calibrated: close calls are flagged **review** and show both candidates rather than a confidently wrong single answer.

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
│    POST /api/3/search/jql   ← completed today        │
│    POST /api/3/search/jql   ← SLA breach count       │
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
- Show/hide/settings transitions share a **single alpha-fade animator** (GPU-composited, no layout redraws, no competing animations)
- Config writes are **atomic** (temp file + rename) and a corrupt config file is quarantined to `.bad` instead of crashing the app

---

## Troubleshooting

| Symptom | Fix |
|---|---|
| "HTTP 401 — check credentials" | Regenerate your API token and paste it via **Settings → Reconfigure credentials** |
| SLA % never appears | Enter the exact SLA field name from your Jira instance in Settings (e.g. `Time to first response`) |
| Queues missing | Check **Settings → Hide queues whose name contains** or the max-ticket threshold |
| Overlay appears on wrong monitor | Drag it to the right monitor — it saves position automatically |
| Notifications not appearing | Check Windows Focus Assist / Do Not Disturb settings |

---

## License

MIT — do whatever you like with it.
