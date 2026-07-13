# Jira Overlay

A lightweight Windows desktop overlay that monitors your **Jira Service Management** queues in real time. It sits quietly in the corner of your screen, pops up when new tickets arrive, and gets out of the way when there's nothing to act on.

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
| Dismiss | ⊟ hides the overlay until a **new** ticket arrives — it won't nag you about tickets you've already seen |
| Snooze | 15 min / 30 min / 1 hour — accessible via the 💤 button |
| Rate-limit aware | Honours `Retry-After` on HTTP 429 and treats 5xx as transient — your data never vanishes over a server hiccup |
| Completed today | Count of tickets closed today with SLA compliance % |
| Corner snapping | Drag near a corner to snap — on **any monitor**, respecting the taskbar. Double-click the header to snap to nearest corner |
| Inline settings | Click ⚙ in the overlay header — expands with a smooth fade, no separate window |
| System tray icon | Badge shows the count for whichever queue you choose; right-click for quick actions |
| Always-visible mode | Optional dashboard mode that keeps the overlay on screen permanently |
| Multi-monitor aware | Clamps position to whichever monitor it's on; detects if it's been moved off-screen |
| Auto-installs dependencies | First run installs `requests`, `Pillow`, and `pystray` automatically |
| Windows startup | Optional — toggle in Settings |
| Fully configurable | Every filter, threshold, SLA field name, and queue is configurable per-user |

---

## Requirements

- **Windows 10 or 11**
- **Python 3.9+** — [python.org](https://python.org)
- A **Jira Service Management** account with API access
- An **API token** — generate one at [id.atlassian.com/manage-profile/security/api-tokens](https://id.atlassian.com/manage-profile/security/api-tokens)

> Third-party packages (`requests`, `Pillow`, `pystray`) are installed automatically on first run — no manual pip step needed.

---

## Installation

```
git clone https://github.com/p3nicillin/Jira-Overlay.git
cd Jira-Overlay
```

Then double-click **`launch_overlay.bat`** — or run:

```
pythonw jira_overlay.pyw
```

---

## First-run setup

1. **Dependency install** — a progress window appears and installs the required packages silently. Closes itself when done.

2. **Credentials** — a setup dialog asks for:
   - **Jira Domain** e.g. `yourcompany.atlassian.net`
   - **Email** your Atlassian account email
   - **API Token** click the 🔑 link in the dialog to open the token page directly

3. **Service desk** — if your account has access to multiple service desks, a picker appears. Click the one you want to monitor.

4. **Alert queue** — a list of all your queues appears. Click one to set your initial alert queue (the one that triggers popups and notifications). You can add more alert queues later in Settings.

Config is saved to `jira_config.json` next to the script. This file is in `.gitignore` and will never be committed.

---

## Usage

Once running, the overlay lives in your **system tray**. It only appears on screen when there's something to act on.

### Overlay controls

| Element | Action |
|---|---|
| **● YOUR DESK NAME** (header) | Click → opens the service desk in your browser |
| **Queue row** | Click → opens that queue in Jira |
| **✓ Completed today — N** | Click → opens today's resolved tickets in Jira with the matching JQL |
| **↻** | Refresh now |
| **💤** | Snooze picker (15 min / 30 min / 1 hour). Click again while snoozed to cancel |
| **⊟** | Dismiss — hide until a **new** ticket arrives |
| **✕** | Quit |
| **⚙** | Open inline settings (expands overlay with a fade animation) |
| Drag | Move the overlay anywhere on screen |
| Double-click header | Snap to nearest corner |

### Tray icon

Click the tray icon to show/hide the overlay (a tray-shown overlay stays up until you hide it). Right-click for Show/Hide and Quit.

The badge count shows whichever queue you designate in Settings → *Tray badge shows*.

---

## Settings

Click **⚙** in the overlay to open Settings inline. All changes take effect on Save.

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

All other keys are populated automatically once the overlay has run.

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
                 │ root.after(0, _update_ui)
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

- All Jira calls run on a **daemon background thread** so the UI never blocks
- HTTP 429 responses honour the server's `Retry-After`; 5xx and network errors keep the last data and show a subtle *Reconnecting…* status
- The overlay uses `overrideredirect(True)` — no title bar, no taskbar entry
- Show/hide/settings transitions share a **single alpha-fade animator** (GPU-composited, no layout redraws, no competing animations)
- Config writes are **atomic** (temp file + rename) and a corrupt config file is quarantined to `.bad` instead of crashing the app

---

## Triage Shadow Learner (optional companion)

`triage_learner.pyw` is a separate, **strictly read-only** background process that learns how your team triages tickets — it never transitions, clones, comments on, or edits anything.

It watches for ITS tickets being routed to **With BI / With DATA / With DEV** (after the Elements Sync clone to BI-/DP-/DEVW- is created) — plus the fourth outcome: **plain IT tickets** that are never cloned or moved and are simply resolved inside the TAC. For each new ticket it:

- **Backfills** from the last 90 days of routing history to seed its model
- **Predicts** a destination for every new ticket at intake and logs *"what I would have done"*
- **Scores itself** when a human routes the ticket — comparing its prediction to the real decision teaches it what's right and wrong
- Regenerates **`TRIAGE_REPORT.md`** with the learned routing map, per-team keywords, and running prediction accuracy

Run it with **`launch_learner.bat`** — it lives as a purple tray icon (click to open the report, right-click → Quit). It shares `jira_config.json` with the overlay.

Its data files (`triage_state.json`, `triage_log.jsonl`, `TRIAGE_REPORT.md`, `triage_learner.log`) contain internal ticket summaries and are gitignored — they stay on your machine.

---

## Troubleshooting

| Symptom | Fix |
|---|---|
| "HTTP 401 — check credentials" | Regenerate your API token and paste it via **⚙ → Reconfigure credentials** |
| SLA % never appears | Enter the exact SLA field name from your Jira instance in Settings (e.g. `Time to first response`) |
| Queues missing | Check **Settings → Hide queues whose name contains** or the max-ticket threshold |
| Overlay appears on wrong monitor | Drag it to the right monitor — it saves position automatically |
| Notifications not appearing | Check Windows Focus Assist / Do Not Disturb settings |

---

## License

MIT — do whatever you like with it.
