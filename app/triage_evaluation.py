"""Shared chronological replay and tuning engine used live and offline."""

from __future__ import annotations

import math
from typing import Optional
from triage_support import segment_name


def bind_engine(Learner, TUNING, features_for, tokenize, BALANCED_TUNED_CONFIG):
    """Bind model dependencies without importing either desktop application."""

    class EvaluationMixin:
        @staticmethod
        def _fit_example(learner, record):
            """One predict-then-learn step, shared by replay and nightly refit."""
            req_type = record.get("requestType", record.get("reqType", ""))
            tokens = features_for(record, req_type)
            title = tokenize(record["summary"])
            reporter = record.get("reporter", "")
            prediction = learner.predict_ex(tokens, req_type, reporter, exemplar=title)
            mistake = prediction["dest"] is not None and prediction["dest"] != record["dest"]
            learner.learn(record["dest"], tokens, req_type, record.get("cloneProjects", []),
                          reporter, weight=int(TUNING.get("mistakeWeight", 1)) if mistake else 1,
                          exemplar=title, prediction=prediction)
            return prediction

        def _replay_history(
            self,
            recs: list[dict],
            cfg: dict,
            score_start: Optional[int] = None,
            score_end: Optional[int] = None,
            include_model: bool = False,
        ) -> dict:
            """Chronologically predict-then-learn over the labelled history under
            `cfg`.

            Return headline accuracy plus balanced/worst-class accuracy so tuning
            cannot hide weak minority-route performance behind high-volume wins.
            """
            saved = dict(TUNING)
            TUNING.update(cfg)
            try:
                lr = Learner({}, escalation_dests=self.dests)
                warm = min(150, len(recs) // 5)
                start = warm if score_start is None else score_start
                end = len(recs) if score_end is None else score_end
                n = ok = firm = firm_ok = 0
                per: dict[str, list[int]] = {}
                firm_per: dict[str, list[int]] = {}
                predicted_per: dict[str, list[int]] = {}
                firm_predicted_per: dict[str, list[int]] = {}
                source_per: dict[str, list[int]] = {}
                segments = {}
                samples = []
                origins = {}
                for i, r in enumerate(recs):
                    req_type = r.get("requestType", r.get("reqType", ""))
                    p = self._fit_example(lr, r)
                    if p["dest"] is not None and start <= i < end:
                        hit = int(p["dest"] == r["dest"])
                        n += 1
                        ok += hit
                        seg = segments.setdefault(segment_name(req_type), [0, 0])
                        seg[0] += 1
                        seg[1] += hit
                        origin = r.get("featureOrigin", "outcome")
                        origins[origin] = origins.get(origin, 0) + 1
                        samples.append(
                            {
                                "actual": r["dest"],
                                "predicted": p["dest"],
                                "conf": p["conf"],
                                "margin": p["margin"],
                                "review": p["review"],
                                "correct": hit,
                                "key": r.get("key", ""),
                                "summary": r["summary"],
                            }
                        )
                        row = per.setdefault(r["dest"], [0, 0])
                        row[0] += 1
                        row[1] += hit
                        prow = predicted_per.setdefault(p["dest"], [0, 0])
                        prow[0] += 1
                        prow[1] += hit
                        srow = source_per.setdefault(p.get("source", "model"), [0, 0])
                        srow[0] += 1
                        srow[1] += hit
                        if not p["review"]:
                            firm += 1
                            firm_ok += hit
                            frow = firm_per.setdefault(r["dest"], [0, 0])
                            frow[0] += 1
                            frow[1] += hit
                            fprow = firm_predicted_per.setdefault(p["dest"], [0, 0])
                            fprow[0] += 1
                            fprow[1] += hit
                per_acc = {k: v[1] / v[0] for k, v in per.items() if v[0]}
                firm_per_acc = {k: v[1] / v[0] for k, v in firm_per.items() if v[0]}
                precision = {k: v[1] / v[0] for k, v in predicted_per.items() if v[0]}
                firm_precision = {
                    k: v[1] / v[0] for k, v in firm_predicted_per.items() if v[0]
                }
                labels = set(per) | set(predicted_per)
                f1 = {}
                for label in labels:
                    recall = per_acc.get(label, 0.0)
                    prec = precision.get(label, 0.0)
                    f1[label] = (
                        2 * prec * recall / (prec + recall) if prec + recall else 0.0
                    )
                source_acc = {
                    k: {"n": v[0], "acc": v[1] / v[0]}
                    for k, v in source_per.items()
                    if v[0]
                }
                return {
                    **({"model": lr.m} if include_model else {}),
                    "acc": ok / n if n else 0.0,
                    "segments": segments,
                    "featureOrigins": origins,
                    "samples": samples,
                    "firmAcc": firm_ok / firm if firm else 0.0,
                    "firmCov": firm / n if n else 0.0,
                    "n": n,
                    "balanced": (sum(per_acc.values()) / len(per_acc))
                    if per_acc
                    else 0.0,
                    "worst": min(per_acc.values(), default=0.0),
                    "per": per_acc,
                    "perCounts": per,
                    "precision": precision,
                    "precisionCounts": predicted_per,
                    "macroPrecision": (
                        sum(precision.get(k, 0.0) for k in labels) / len(labels)
                    )
                    if labels
                    else 0.0,
                    "worstPrecision": min(
                        (precision.get(k, 0.0) for k in labels), default=0.0
                    ),
                    "f1": f1,
                    "macroF1": (sum(f1.values()) / len(f1)) if f1 else 0.0,
                    "worstF1": min(f1.values(), default=0.0),
                    "firmBalanced": (sum(firm_per_acc.values()) / len(firm_per_acc))
                    if firm_per_acc
                    else 0.0,
                    "firmWorst": min(firm_per_acc.values(), default=0.0),
                    "firmPer": firm_per_acc,
                    "firmPerCounts": firm_per,
                    "firmPrecision": firm_precision,
                    "firmPrecisionCounts": firm_predicted_per,
                    "firmMacroPrecision": (
                        sum(firm_precision.values()) / len(firm_precision)
                    )
                    if firm_precision
                    else 0.0,
                    "firmWorstPrecision": min(firm_precision.values(), default=0.0),
                    "sources": source_acc,
                }
            finally:
                TUNING.clear()
                TUNING.update(saved)

        # Values the nightly tune is allowed to explore, per knob. Searched by
        # coordinate descent rather than the old fixed list of 13 hand-written
        # combinations, which could only ever move one or two knobs at a time.
        _TUNE_AXES = {
            "routeBalanceWeight": [0.0, 0.5, 1.0, 2.0],
            "summaryWeight": [3, 4, 5, 6, 8, 10],
            "mistakeWeight": [1, 2, 3, 4],
            "decayFactor": [1.0, 0.999, 0.997, 0.995, 0.99],
            "confTemp": [2.0, 3.0, 4.0, 6.0, 8.0],
            "priorWeight": [0.0, 0.25, 0.5, 0.75, 1.0],
            "knnThresh": [0.0, 0.6, 0.7, 0.8, 0.9],
            "requestTypeHintWeight": [0.0, 1.5, 3.0, 5.0],
            # Who raised a ticket predicts the receiving team on its own far
            # better than chance, but contributes a single log term against one
            # per token. Whether amplifying it actually helps was inconclusive on
            # a 400-ticket holdout — a real peak, but worth about one ticket — so
            # it is offered to the tuner at a default of 1.0 (today's behaviour)
            # rather than hard-coded, and adopted only if the evidence grows.
            "reporterWeight": [0.0, 1.0, 2.0, 3.0, 5.0],
            "escalationSpecialist": [0, 1],
            "escalationPriorWeight": [0.0, 0.25, 0.5, 0.75, 1.0],
            "reviewConf": [65, 70, 75, 80, 85],
            "reviewMargin": [10, 15, 20, 25],
        }
        _TUNE_KEEP = tuple(_TUNE_AXES)
        _TUNE_BUDGET = 120  # replays per night — ~1.5 min at current size
        _TUNE_PASSES = 3

        @staticmethod
        def _tune_score(m: dict) -> tuple:
            """Rank candidates by macro F1, then recall and precision.

            Balanced recall alone can look excellent while a rare route sprays
            false positives into common classes. Macro F1 makes both sides count;
            confident-band precision then decides between otherwise similar
            candidates.
            """
            # Metric fallbacks keep saved/offline replay fixtures from older
            # schemas comparable while new replays always provide the richer set.
            return (
                round(m.get("macroF1", m["balanced"]), 3),
                round(m["balanced"], 3),
                round(m.get("macroPrecision", m["balanced"]), 3),
                round(m.get("worstF1", m["worst"]), 3),
                round(m["worst"], 3),
                round(m.get("firmMacroPrecision", m["firmBalanced"]), 3),
                round(m.get("firmWorstPrecision", m["firmWorst"]), 3),
                round(m["firmBalanced"], 3),
                round(m["firmWorst"], 3),
                round(m["firmAcc"], 3),
                m["firmCov"],
                round(m["acc"], 3),
            )

        def _self_tune(self, recs: list[dict]) -> Optional[dict]:
            """Tune, gate against the incumbent, then report on an untouched tail."""
            incumbent = {k: TUNING[k] for k in self._TUNE_KEEP}
            # Validate across roughly three recent 400-ticket windows. A single
            # 400-ticket tail repeatedly contained no PPS/IPS outcomes, making a
            # 91% headline say nothing at all about those routes. The wider
            # chronological span remains untouched during config selection but
            # is much more likely to include every active outcome.
            validation_n = max(100, min(1200, len(recs) // 3))
            split = len(recs) - validation_n
            tuning_recs = recs[:split]

            cache: dict[tuple, tuple] = {}
            spent = 0

            def evaluate(cfg: dict, budget: list[int]):
                """Scored replay of `cfg`, memoised; None once out of budget.

                `budget` is this descent's own remaining allowance, not the
                shared total — a cache hit costs nothing, but a fresh replay
                draws only from the caller's share, so one hungry descent can't
                spend the whole night's budget before the other gets to run.
                """
                nonlocal spent
                sig = tuple(cfg[k] for k in self._TUNE_KEEP)
                if sig in cache:
                    return cache[sig]
                if budget[0] <= 0:
                    return None
                budget[0] -= 1
                spent += 1
                # Fit on the full prefix, but rank by its recent tail. Old,
                # repetitive successes must not drown out current weaknesses.
                recent_start = max(min(150, len(tuning_recs)//5), len(tuning_recs)-1200)
                m = self._replay_history(tuning_recs, cfg, score_start=recent_start)
                cache[sig] = (self._tune_score(m), m)
                return cache[sig]

            def descend(seed: dict, budget: list[int]):
                """Coordinate descent from `seed`: sweep each knob against the
                current best, keep any improvement, repeat until a full pass
                changes nothing."""
                cur = dict(seed)
                got = evaluate(cur, budget)
                if got is None:
                    return None
                best_score, best_m = got
                for _ in range(self._TUNE_PASSES):
                    improved = False
                    for axis, values in self._TUNE_AXES.items():
                        for value in values:
                            if cur.get(axis) == value:
                                continue
                            trial = dict(cur, **{axis: value})
                            got = evaluate(trial, budget)
                            if got is None:
                                break
                            score, m = got
                            if score > best_score:
                                best_score, best_m, cur = score, m, trial
                                improved = True
                        if budget[0] <= 0:
                            break
                    if not improved or budget[0] <= 0:
                        break
                return best_score, best_m, cur

            # Greedy descent only ever explores the basin it starts in: once a
            # knob such as decayFactor settles near last night's value, testing
            # it alone rarely looks better, even when a very different value
            # would win once paired with other knobs the search already moved
            # away from. Descending twice - once warm-started from tonight's
            # live config, once from the fixed, offline-validated baseline -
            # and keeping whichever finishes with the better score is what
            # surfaced decayFactor=1.0 (never forget) as far better for the
            # low-volume BI class than the 0.999 the live config was stuck on.
            # A single descent alone can exhaust the whole budget before
            # converging, so each gets its own guaranteed half rather than
            # sharing one pool the first descent could spend in full.
            half = max(1, self._TUNE_BUDGET // 2)
            runs = [
                descend(dict(TUNING), [half]),
                descend(dict(BALANCED_TUNED_CONFIG), [self._TUNE_BUDGET - half]),
            ]
            runs = [r for r in runs if r is not None]
            if not runs:
                return None
            best_score, best_m, cur = max(runs, key=lambda r: r[0])

            challenger = {k: cur[k] for k in self._TUNE_KEEP}

            # Use the first two recent windows as a champion/challenger gate and
            # leave the final ~400 decisions untouched. Previously, a candidate
            # could win on the tuning history, score materially worse on the
            # holdout, and still be deployed; the holdout was reporting the
            # regression rather than preventing it.
            final_n = min(400, max(100, validation_n // 3))
            final_start = len(recs) - final_n
            gate_end = max(split, final_start)
            challenger_gate = self._replay_history(
                recs, challenger, score_start=split, score_end=gate_end
            )
            incumbent_gate = self._replay_history(
                recs, incumbent, score_start=split, score_end=gate_end
            )
            rejected = not self._accept_challenger(incumbent_gate, challenger_gate)
            chosen = incumbent if rejected else challenger
            gate_reason = (
                "Challenger did not improve the chronological gate within accuracy safeguards"
                if rejected
                else "Challenger beat incumbent on chronological gate"
            )

            best = {
                "score": best_score,
                "cfg": chosen,
                "train": best_m,
                "trainN": best_m["n"],
                "split": split,
                "evals": spent,
                "validationWindows": max(1, math.ceil(validation_n / 400)),
                "challengerRejected": rejected,
                "gateReason": gate_reason,
                "incumbentGateScore": self._tune_score(incumbent_gate),
                "challengerGateScore": self._tune_score(challenger_gate),
                "challengerCfg": challenger if rejected else None,
                "gate": incumbent_gate if rejected else challenger_gate,
                "gateN": (incumbent_gate if rejected else challenger_gate)["n"],
            }
            if best is not None:
                validation = self._replay_history(recs, best["cfg"], score_start=split)
                final = self._replay_history(recs, best["cfg"], score_start=final_start)
                best["holdout"] = validation
                best["holdoutN"] = validation["n"]
                best["finalHoldout"] = final
                best["finalHoldoutN"] = final["n"]
                for key in (
                    "acc",
                    "balanced",
                    "worst",
                    "firmAcc",
                    "firmBalanced",
                    "firmWorst",
                    "firmCov",
                    "n",
                    "per",
                    "firmPer",
                    "perCounts",
                    "firmPerCounts",
                    "precision",
                    "precisionCounts",
                    "macroPrecision",
                    "worstPrecision",
                    "f1",
                    "macroF1",
                    "worstF1",
                    "firmPrecision",
                    "firmPrecisionCounts",
                    "firmMacroPrecision",
                    "firmWorstPrecision",
                    "sources",
                    "segments",
                    "featureOrigins",
                ):
                    if key in validation:
                        best[key] = validation[key]
            return best

        @classmethod
        def _accept_challenger(cls, incumbent, challenger):
            """A macro-score win cannot purchase a material accuracy regression."""
            if cls._tune_score(challenger) <= cls._tune_score(incumbent):
                return False
            if challenger["acc"] < incumbent["acc"] - 0.005:
                return False
            return challenger["firmAcc"] >= incumbent["firmAcc"] - 0.005

    return EvaluationMixin
