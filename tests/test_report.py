"""Team report: period maths, JQL, ranking, and the config it saves."""
import json
import os
import sys
from datetime import date
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _harness import load_overlay                    # noqa: E402
ov = load_overlay()
json.dump({}, open(ov.CONFIG_FILE, "w"))
ok = True
def check(n, c, d=""):
    global ok
    print(("PASS  " if c else "FAIL  ") + n + (f"   {d}" if d else "")); ok = ok and c

# --- report_users: config shapes and the seed-from-filter fallback ---------
R = ov.report_users
check("reads a list of objects",
      R({"reportUsers": [{"name": "Sam", "accountId": "a1"}]})
      == [{"name": "Sam", "accountId": "a1"}])
check("accepts a plain list of names (hand-edited config)",
      R({"reportUsers": ["Sam", "Ada"]})
      == [{"name": "Sam", "accountId": ""}, {"name": "Ada", "accountId": ""}])
seeded = R({"completedTodayFilter": 'assignee in ("Sam Carter", "Ada Nkemi")'})
check("seeds from an existing assignee filter",
      [p["name"] for p in seeded] == ["Sam Carter", "Ada Nkemi"])
check("empty config gives nobody", R({}) == [])

# --- period maths ----------------------------------------------------------
import tkinter as tk
root = tk.Tk(); root.withdraw()
w = object.__new__(ov.TeamReportWindow)
w.v_range = tk.StringVar(); w.v_from = tk.StringVar(); w.v_to = tk.StringVar()
today = date.today()
for label, days in ov.REPORT_RANGES:
    if days is None: continue
    w.v_range.set(label)
    start, end = w._period()
    good = end == today and (end - start).days == days
    check(f"range '{label}' spans {days + 1} day(s)", good, f"{start}..{end}")
w.v_range.set("Custom")
w.v_from.set("2026-01-10"); w.v_to.set("2026-01-20")
check("custom dates honoured", w._period() == (date(2026,1,10), date(2026,1,20)))
w.v_from.set("2026-02-01"); w.v_to.set("2026-01-01")
check("back-to-front dates are swapped, not rejected",
      w._period() == (date(2026,1,1), date(2026,2,1)))
w.v_from.set("not a date")
check("nonsense dates report rather than crash", w._period() is None)

# --- the JQL actually issued ----------------------------------------------
seen = []
class App:
    config = {"projectKey": "SD", "serviceDeskName": "Desk"}
    def _count_issues(self, jql): seen.append(jql); return (7, False)
    def _post(self, fn): fn()
w.app = App(); w._closed = False; w._seq = 1
w.v_basis = tk.StringVar(value="resolved")
w.people = [{"name": "Sam Carter", "accountId": "acc-1"},
            {"name": "Ada", "accountId": ""}]
w._done = lambda seq, results, total, *_coverage: seen.append(
    ("RESULT", results, total))
w._work(1, date(2026, 1, 1), date(2026, 1, 31))
q = seen[0]
check("counts only completed work", "statusCategory = Done" in q)
check("scopes to the project", "project = SD" in q)
check("start of window inclusive", 'resolved >= "2026-01-01"' in q)
check("end of window covers the final whole day",
      'resolved < "2026-02-01"' in q, q)
check("uses accountId when known", 'assignee = "acc-1"' in q)
check("falls back to display name", 'assignee = "Ada"' in seen[1])
res = [x for x in seen if isinstance(x, tuple) and x[0] == "RESULT"][0]
check("totals across everyone", res[2] == 14, str(res[2]))

# --- basis switch ----------------------------------------------------------
seen.clear(); w.v_basis.set("updated"); w._seq = 2
w._work(2, date(2026, 1, 1), date(2026, 1, 2))
check("'Last updated' basis changes the field", 'updated >= "2026-01-01"' in seen[0])

# --- Names are user-supplied text, and they go into a query ----------------
# Display names are set by the people they belong to, so quotes and
# backslashes turn up legitimately. Interpolated raw, a name either breaks
# the query or changes what it counts.
QUOTE, SLASH = '"', chr(92)
check("a quote in a name is escaped",
      ov.jql_quote("O" + QUOTE + "Brien")
      == QUOTE + "O" + SLASH + QUOTE + "Brien" + QUOTE,
      ov.jql_quote("O" + QUOTE + "Brien"))
check("a backslash is escaped",
      ov.jql_quote("DOMAIN" + SLASH + "user")
      == QUOTE + "DOMAIN" + SLASH * 2 + "user" + QUOTE,
      ov.jql_quote("DOMAIN" + SLASH + "user"))
check("an injection attempt stays a single literal",
      ov.jql_quote('x" OR project=OTHER OR "').count(SLASH) == 2,
      ov.jql_quote('x" OR project=OTHER OR "'))

seen.clear()
w.people = [{"name": "O" + QUOTE + "Brien", "accountId": ""}]
w._seq += 1
w._work(w._seq, date(2026, 1, 1), date(2026, 1, 2))
check("the report escapes the name it queries with",
      SLASH + QUOTE + "Brien" in seen[0], seen[0][-44:])

seen.clear()
w.people = [{"name": "", "accountId": ""}]
w._seq += 1
w._work(w._seq, date(2026, 1, 1), date(2026, 1, 2))
check("a person with no name costs no per-person query",
      not [q for q in seen if "assignee =" in str(q)],
      str([q for q in seen if "assignee =" in str(q)]))

# --- The People picker, matching two different sources ---------------------
# People seeded from a completed-today filter have a name and no accountId.
# Jira returns an accountId. Comparing "accountId or name" on both sides
# meant they never matched, so everyone already chosen was listed twice -
# once ticked, once not.
JIRA_USERS = [
    {"name": "Ada Nkemi",    "accountId": "acc-ada"},
    {"name": "Bart Bielski", "accountId": "acc-bart"},
    {"name": "Jo Bright",    "accountId": "acc-jo"},
    {"name": "Sam Carter",   "accountId": "acc-sam"},
]
SEEDED = [{"name": n, "accountId": ""}
          for n in ("Ada Nkemi", "Jo Bright", "Sam Carter")]
GONE = [{"name": "Ex Employee", "accountId": ""}]

listed, ticked = ov._PeoplePicker._merge(SEEDED + GONE, list(JIRA_USERS))

names = [p["name"] for p in listed]
check("nobody is listed twice", len(names) == len(set(names)), str(names))
check("everyone already chosen is ticked",
      sorted(p["name"] for p in listed if ticked(p))
      == sorted(["Ada Nkemi", "Ex Employee", "Jo Bright", "Sam Carter"]),
      str(sorted(p["name"] for p in listed if ticked(p))))
check("people not chosen stay unticked",
      not ticked({"name": "Bart Bielski", "accountId": "acc-bart"}))
check("someone who has left Jira is still listed",
      "Ex Employee" in names)
check("a matched person carries Jira's accountId, not the seeded blank",
      next(p for p in listed if p["name"] == "Ada Nkemi")["accountId"]
      == "acc-ada")

# --- Coverage: how much of the completed work the ranking accounts for ----
# A ranking shown with no sense of scale is misleading on a desk where most
# completed tickets are unassigned - which, on a desk that clones a ticket
# out to another project rather than assigning the master, is normal rather
# than a sign anyone is slacking. A fresh window is used here rather than
# the one above, whose _done was overridden earlier in this file.
w2 = object.__new__(ov.TeamReportWindow)
w2.v_range = tk.StringVar(value="Custom")
w2.v_from = tk.StringVar()
w2.v_to = tk.StringVar()
w2.v_basis = tk.StringVar(value="resolved")
w2._closed = False
w2._seq = 1
w2._render = lambda *a: None


def coverage_status(total_completed, unassigned):
    w2.people = [{"name": "Alfie", "accountId": ""},
                 {"name": "Luke", "accountId": ""}]
    counts = {"Alfie": 291, "Luke": 235}          # sums to 526
    status = {}

    class App:
        config = {"projectKey": "SD"}

        def _count_issues(self, jql):
            if "assignee IS EMPTY" in jql:
                return (unassigned, False)
            if "assignee" not in jql:
                return (total_completed, False)
            for name, n in counts.items():
                if f'"{name}"' in jql:
                    return (n, False)
            return (0, False)

        def _post(self, fn):
            fn()

    w2.app = App()
    w2._set_status = lambda text, warn=False: status.update(text=text, warn=warn)
    w2._seq += 1
    w2._work(w2._seq, date(2026, 1, 1), date(2026, 1, 31))
    return status


# 526 of 1028 is 51% - above the warning threshold.
s = coverage_status(1028, 436)
check("the status line states coverage as a fact, not a verdict",
      "526 of 1028" in s["text"] and "51%" in s["text"], s["text"])
check("the unassigned count is shown", "436" in s["text"], s["text"])
check("51% coverage doesn't trip the low-coverage colour", not s["warn"])

s = coverage_status(600, 0)
check("zero unassigned omits the 'had no assignee' clause",
      "assignee" not in s["text"], s["text"])

s = coverage_status(2000, 1900)
check("well under half attributed does flag the colour", s["warn"])

s = coverage_status(None, None)
check("if Jira refuses the extra queries, the total alone is still shown",
      "completed in total" in s["text"], s["text"])


# --- CSV export: a name is not safe to write verbatim ----------------------
# A display name starting with = + - @ or a tab is a valid Jira name that
# Excel will still try to evaluate as a formula - "CSV injection", and not
# merely theoretical: some Excel configurations honour old DDE syntax inside
# such a formula. The standard fix is a leading single quote, which Excel
# shows the text after and drops rather than evaluating.
for name in ("=1+1", "+SUM(A1:A9)", "-2+3", "@SUM(1,1)", "\tcmd", "-"):
    safe = ov.csv_safe(name)
    check(f"{name!r} can't be read as a formula after export",
          safe[0] == "'" and safe[1:] == name, safe)
for name in ("Sam Carter", "O'Brien", ""):
    check(f"{name!r} is left alone", ov.csv_safe(name) == name)

root.destroy()
print("\nRESULT:", "ALL PASS" if ok else "FAILURES PRESENT")
sys.exit(0 if ok else 1)
