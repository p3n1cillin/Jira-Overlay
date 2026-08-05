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
w._done = lambda seq, results, total: seen.append(("RESULT", results, total))
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

root.destroy()
print("\nRESULT:", "ALL PASS" if ok else "FAILURES PRESENT")
sys.exit(0 if ok else 1)
