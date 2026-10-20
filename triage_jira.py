"""Read-only Jira transport, isolated from desktop and model code."""

from __future__ import annotations

import threading
import time
from typing import Optional


def make_client(requests, HTTPBasicAuth, json_objects, log_error):
    class Jira:
        """Thin read-only wrapper. GET + search only — no mutations, ever."""

        def __init__(self, cfg: dict) -> None:
            self.domain = cfg["domain"]
            self.session = requests.Session()
            self.session.auth = HTTPBasicAuth(cfg["email"], cfg["token"])
            self.session.headers["Accept"] = "application/json"
            # set by the watcher so a long retry backoff doesn't delay Quit
            self.stop: Optional[threading.Event] = None
            self._refusals: set[int] = set()  # auth/permission codes already logged
            self.incomplete_reads = 0
            self.last_success_epoch = 0.0

        def _pause(self, seconds: float) -> bool:
            """Sleep between retries; True if we were asked to stop instead."""
            if self.stop is not None:
                return self.stop.wait(seconds)
            time.sleep(seconds)
            return False

        def _url(self, path: str) -> str:
            return f"https://{self.domain}{path}"

        def _send(
            self,
            method: str,
            path: str,
            *,
            params: Optional[dict] = None,
            json_body: Optional[dict] = None,
            timeout: int = 20,
        ):
            """One request with retry/backoff. Backfill fires thousands of calls,
            so Jira can rate-limit or reset connections (WinError 10054); without
            this the whole cycle would abort and no suggestions get written."""
            url = self._url(path)
            delay = 1.0
            for attempt in range(5):
                try:
                    r = self.session.request(
                        method, url, params=params, json=json_body, timeout=timeout
                    )
                except (requests.ConnectionError, requests.Timeout):
                    if attempt == 4 or self._pause(delay):
                        return None
                    delay = min(delay * 2, 20)
                    continue
                if r.status_code == 429 or r.status_code >= 500:
                    if attempt == 4:
                        return None
                    ra = r.headers.get("Retry-After", "")
                    if self._pause(float(ra) if ra.isdigit() else delay):
                        return None
                    delay = min(delay * 2, 20)
                    continue
                self._note_refusal(r)
                return r
            return None

        def _note_refusal(self, response) -> None:
            """Record the first auth/permission refusal of each kind.

            Otherwise a wrong or expired token just makes every search come back
            empty, for ever, with nothing written anywhere — the learner appears
            to run perfectly and simply never produce a suggestion.
            """
            status = response.status_code
            path = response.request.path_url
            if status < 400:
                self.last_success_epoch = time.time()
                # A successful search demonstrates project access has recovered.
                if "/search" in path:
                    self._refusals.clear()
                return
            # Missing/deleted individual tickets are not a project-wide outage.
            if status == 404 and "/issue/" in path:
                return
            if status not in (401, 403, 404) or status in self._refusals:
                return
            self._refusals.add(status)
            reason = {
                401: "credentials rejected — check the API token in jira_config.json",
                403: "permission denied — this account cannot read that project",
                404: "not found — check projectKey in jira_config.json",
            }[status]
            log_error(
                f"Jira returned {status} for "
                f"{response.request.method} {response.request.path_url}: "
                f"{reason}"
            )

        def get(self, path: str, params: Optional[dict] = None) -> Optional[dict]:
            r = self._send("GET", path, params=params, timeout=15)
            if r is None or not r.ok:
                return None
            try:
                return r.json()
            except ValueError:
                self.incomplete_reads = getattr(self, "incomplete_reads", 0) + 1
                return None

        def search(
            self, jql: str, fields: list[str], cap: Optional[int] = 100
        ) -> list[dict]:
            """POST /search/jql is Jira's read-only search — no data is modified."""
            issues: list[dict] = []
            token: Optional[str] = None
            tokens_seen = set()
            complete = False
            while cap is None or len(issues) < cap:
                stop = getattr(self, "stop", None)
                if stop is not None and stop.is_set():
                    break
                body: dict = {
                    "jql": jql,
                    "fields": fields,
                    "maxResults": 100 if cap is None else min(100, cap - len(issues)),
                }
                if token:
                    body["nextPageToken"] = token
                r = self._send(
                    "POST", "/rest/api/3/search/jql", json_body=body, timeout=20
                )
                if r is None or not r.ok:
                    break
                try:
                    data = r.json()
                except ValueError:  # a 200 that isn't JSON at all
                    break
                if not isinstance(data, dict):
                    break
                # json_objects, not `or []`: an "issues" that is a string would
                # otherwise be iterated one character at a time.
                page = json_objects(data.get("issues"))
                issues.extend(page)
                token = data.get("nextPageToken")
                # Stop on an explicit isLast, an empty page, or no next token —
                # not on isLast merely being ABSENT: a server that paginates
                # with tokens but omits the flag would otherwise end the search
                # after one page.
                if not token or data.get("isLast") is True:
                    complete = True
                    break
                if not page or token in tokens_seen:
                    break
                tokens_seen.add(token)
            if not complete:
                self.incomplete_reads = getattr(self, "incomplete_reads", 0) + 1
            return issues

        def changelog(self, key: str) -> list[dict]:
            """All changelog histories for an issue (paginated GET)."""
            out: list[dict] = []
            start = 0
            while True:
                data = self.get(
                    f"/rest/api/3/issue/{key}/changelog",
                    {"startAt": start, "maxResults": 100},
                )
                if not isinstance(data, dict):
                    self.incomplete_reads = getattr(self, "incomplete_reads", 0) + 1
                    break
                page = json_objects(data.get("values"))
                out.extend(page)
                # Stop on an empty page as well as isLast: a response that keeps
                # claiming there is more, without returning any, would otherwise
                # page for ever.
                if data.get("isLast", True):
                    break
                if not page:
                    self.incomplete_reads = getattr(self, "incomplete_reads", 0) + 1
                    break
                start += len(page)
            return out

    return Jira
