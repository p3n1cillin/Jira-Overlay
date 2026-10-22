"""Team report: period maths, JQL, ranking, and the config it saves."""
import json
import os
import sys
import time
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

# The same human can legitimately be listed twice — seeded from the
# completed-today filter and then also ticked from Jira, or hand-edited.
# Counted twice they get a double-length bar, inflate the total, and push
# the coverage figure past 100%, which is nonsense on its face.
for label, users, expected in (
        ("the same name twice",
         [{"name": "Sam", "accountId": ""}, {"name": "Sam", "accountId": ""}], 1),
        ("one accountId under two names",
         [{"name": "Sam", "accountId": "a1"},
          {"name": "Sam Carter", "accountId": "a1"}], 1),
        ("the same name with and without an accountId",
         [{"name": "Sam", "accountId": ""},
          {"name": "Sam", "accountId": "a1"}], 1),
        ("the same name in different case",
         [{"name": "sam carter", "accountId": ""},
          {"name": "Sam Carter", "accountId": ""}], 1),
        ("two genuinely different people",
         [{"name": "Sam", "accountId": "a1"},
          {"name": "Ada", "accountId": "a2"}], 2)):
    got = R({"reportUsers": users})
    check(f"{label} -> {expected} person(s)", len(got) == expected,
          str([p["name"] for p in got]))

check("a repeated name in the seeded filter is counted once",
      [p["name"] for p in
       R({"completedTodayFilter": 'assignee in ("Sam","Ada","Sam")'})]
      == ["Sam", "Ada"])

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
    def _count_issues(self, jql, session=None): seen.append(jql); return (7, False)
    def _post(self, fn): fn()
w.app = App(); w._closed = False; w._seq = 1
w.v_basis = tk.StringVar(value="resolved")
w.people = [{"name": "Sam Carter", "accountId": "acc-1"},
            {"name": "Ada", "accountId": ""}]
w._done = lambda seq, results, total, *_coverage: seen.append(
    ("RESULT", results, total))
w._work(1, date(2026, 1, 1), date(2026, 1, 31), "resolved")
# The four-wide pool deliberately makes HTTP request order nondeterministic.
q = next(s for s in seen if isinstance(s, str) and 'assignee = "acc-1"' in s)
check("counts only completed work", "statusCategory = Done" in q)
check("scopes to the project", "project = SD" in q)
check("start of window inclusive", 'resolved >= "2026-01-01"' in q)
check("end of window covers the final whole day",
      'resolved < "2026-02-01"' in q, q)
check("uses accountId when known", 'assignee = "acc-1"' in q)
check("falls back to display name", any(isinstance(s, str) and 'assignee = "Ada"' in s for s in seen))
res = [x for x in seen if isinstance(x, tuple) and x[0] == "RESULT"][0]
check("totals across everyone", res[2] == 14, str(res[2]))

# --- basis switch ----------------------------------------------------------
seen.clear(); w.v_basis.set("updated"); w._seq = 2
w._work(2, date(2026, 1, 1), date(2026, 1, 2), "updated")
check("'Last updated' basis changes the field",
      all('updated >= "2026-01-01"' in s for s in seen if isinstance(s, str)))

# --- reportBasis names a JQL field, not a value - jql_quote() can't help --
# v_basis is seeded straight from config ("reportBasis"), hand-editable
# text, and interpolated as a bare field name (`{basis} >= "..."`), not a
# quoted literal. A value the "Count by" radio buttons never offer must
# still be rejected rather than trusted, the same way an unvalidated column
# name reaching SQL would need to be.
seen.clear(); w.v_basis.set('resolved" OR project = "SECRET'); w._seq = 3
w._work(3, date(2026, 1, 1), date(2026, 1, 2),
        'resolved" OR project = "SECRET')
check("a basis outside {resolved, updated} falls back to a safe default",
      all("SECRET" not in q for q in seen if isinstance(q, str)), seen)
w.v_basis.set("resolved")

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
w._work(w._seq, date(2026, 1, 1), date(2026, 1, 2), "resolved")
name_query = next(s for s in seen if isinstance(s, str) and ' AND assignee = ' in s)
check("the report escapes the name it queries with",
      SLASH + QUOTE + "Brien" in name_query, name_query[-44:])

seen.clear()
w.people = [{"name": "", "accountId": ""}]
w._seq += 1
w._work(w._seq, date(2026, 1, 1), date(2026, 1, 2), "resolved")
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

        def _count_issues(self, jql, session=None):
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
    w2._work(w2._seq, date(2026, 1, 1), date(2026, 1, 31), "resolved")
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

# The per-person counts and the project total are separate queries taken
# moments apart, so a ticket resolved in between can make the parts add to
# more than the whole. A coverage figure above 100% is visibly nonsense and
# discredits the number beside it.
s = coverage_status(300, 100)          # sum 526 against a smaller total
check("coverage never claims more than 100%",
      "(100%)" in s["text"] and "%" in s["text"], s["text"])


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

# --- Counting on a worker thread racing a window close ----------------------
# _work runs on a background thread. It used to read self.v_basis (a
# tk.StringVar) once at the top - a cross-thread Tcl call that only works
# when mainloop continuously serves it, and deadlocks or crashes otherwise.
# refresh() now reads the variable on the main thread and passes `basis`
# in, so _work touches no Tcl state at all; this trial keeps covering the
# close-during-count race itself. A watchdog kills the process rather than
# hanging the suite if this regresses.
import threading                                      # noqa: E402


def _kill_on_hang(seconds):
    def watch():
        time.sleep(seconds)
        print("test_report: _work + close race did not finish in time "
              "-- treating as a hang")
        os._exit(1)
    t = threading.Thread(target=watch, daemon=True)
    t.start()
    return t


TRIALS = 15
done = {"n": 0}
race_errors = []
_kill_on_hang(12)

for trial in range(TRIALS):
    tw = object.__new__(ov.TeamReportWindow)
    tw.v_basis = tk.StringVar(value="resolved")
    tw.people = [{"name": "Sam", "accountId": ""}]
    tw._closed = False
    tw._seq = 1

    class SlowApp:
        config = {"projectKey": "SD"}

        def _count_issues(self, jql, session=None):
            time.sleep(0.005)
            return (1, False)

        def _post(self, fn):
            root.after(0, fn)

    tw.app = SlowApp()
    tw._done = lambda *a, **k: None

    def worker(tw=tw, trial=trial):
        try:
            tw._work(trial, date(2026, 1, 1), date(2026, 1, 2), "resolved")
        except Exception as exc:                      # noqa: BLE001
            race_errors.append((trial, exc))
        finally:
            done["n"] += 1

    threading.Thread(target=worker, daemon=True).start()
    # Close it from the main thread shortly after, the way a click on the
    # window's X actually happens, while mainloop keeps running underneath.
    root.after(2, (lambda tw=tw: setattr(tw, "_closed", True)))


def _pump_until_done():
    if done["n"] >= TRIALS:
        root.quit()
    else:
        root.after(20, _pump_until_done)


root.after(20, _pump_until_done)
root.mainloop()

check(f"{TRIALS} counts racing a close all finish without error",
      not race_errors and done["n"] == TRIALS,
      f"{done['n']}/{TRIALS} finished, errors: {race_errors}")

# --- Rapid Refresh clicks: only the most recent one should ever render -----
# Each refresh() bumps self._seq and starts a background count carrying that
# seq; _done only renders when its seq still matches self._seq. A prior
# version of this test tied "which request is slow" to the GLOBAL ORDER
# _count_issues was called in, rather than to which request it belonged to -
# that doesn't model real latency (an older request isn't guaranteed to
# reach Jira first) and produced a result that looked exactly like a stale-
# data bug. With latency attached to the request itself instead, 15 trials
# of 5 rapid clicks with independently randomised delays never render
# anything but the last click's result.
import random                                          # noqa: E402


def _refresh_race_trial(n_clicks):
    rw = object.__new__(ov.TeamReportWindow)
    rw.v_range = tk.StringVar(value="Today")
    rw.v_from = tk.StringVar()
    rw.v_to = tk.StringVar()
    rw.v_basis = tk.StringVar(value="resolved")
    rw.people = [{"name": "Sam", "accountId": ""}]
    rw._closed = False
    rw._seq = 0
    rw.lbl_sub = type("L", (), {"config": lambda self, **k: None})()
    rw._set_status = lambda *a, **k: None
    renders = []
    rw._render = lambda results, total: renders.append(total)

    class Mock:
        def __init__(self, value, delay):
            self.value, self.delay = value, delay
            self.config = {"projectKey": "SD"}

        def _count_issues(self, jql, session=None):
            time.sleep(self.delay)
            return (self.value, False)

        def _post(self, fn):
            root.after(0, fn)

    for i in range(n_clicks):
        rw.app = Mock(value=i, delay=random.uniform(0, 0.05))
        rw.refresh()

    started = time.perf_counter()

    def finish():
        if time.perf_counter() - started > 0.3:
            root.quit()
        else:
            root.after(10, finish)

    root.after(10, finish)
    root.mainloop()
    return renders


rapid_failures = [(t, r) for t in range(8)
                  if (r := _refresh_race_trial(5)) != [4]]
check("only the latest of 5 rapid clicks ever renders, 8 times over",
      not rapid_failures, str(rapid_failures))

# --- _work counts four-wide rather than one round-trip per person ------------
# A barrier of two proves overlap deterministically: with concurrent counts
# any pair of calls meets it instantly; a serial loop blocks on the first
# wait, times out, and every count comes back None.
import threading                                    # noqa: E402
barrier = threading.Barrier(2)


class StubApp:
    config = {"projectKey": "SD", "email": "e@x", "token": "t"}

    def __init__(self):
        self.calls = []
        self.posted = []

    def _count_issues(self, jql, session=None):
        self.calls.append((jql, session is not None))
        barrier.wait(timeout=5)
        return (7 if "assignee =" in jql else 3), False

    def _post(self, fn):
        self.posted.append(fn)


stub = StubApp()
win = object.__new__(ov.TeamReportWindow)
win.app = stub
win.people = [{"name": "A", "accountId": "a"},
              {"name": "B", "accountId": "b"}]
win._closed = False
win._seq = 1
done_args = []
win._done = lambda *a: done_args.append(a)
ov.TeamReportWindow._work(win, 1, date(2026, 8, 1), date(2026, 8, 26),
                          "resolved")
check("the worker handed exactly one completion to the main thread",
      len(stub.posted) == 1, str(len(stub.posted)))
stub.posted[0]()
_seq, rows, total, completed, unassigned = done_args[0]
check("counts ran concurrently and stayed ordered and correct",
      rows == [("A", 7), ("B", 7)] and total == 14
      and completed == 3 and unassigned == 3, str(done_args))
check("worker threads brought their own session",
      all(own for _jql, own in stub.calls), str(stub.calls))

root.destroy()
print("\nRESULT:", "ALL PASS" if ok else "FAILURES PRESENT")
sys.exit(0 if ok else 1)
