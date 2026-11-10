#!/usr/bin/env python3
"""End-to-end suite for the Envelope desktop shell.

Runs real permutations against a real built app: the bind/auth matrix, the
process lifecycle, and the frontend layout measured in a browser. Nothing is
mocked except Tauri's IPC, which cannot exist outside the app.

Every case uses a throwaway HOME, so this is safe to run on a machine with
real accounts configured.

    python3 apps/desktop/tests/e2e/run.py [--only server|lifecycle|ui]

Prerequisites: `npm run stage-sidecar` and `npm run tauri build`.
Exits non-zero if any case fails.
"""

from __future__ import annotations

import argparse
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import cases_lifecycle  # noqa: E402
import cases_server  # noqa: E402
import cases_ui  # noqa: E402
from harness import Suite  # noqa: E402

GROUPS = {"server": cases_server, "lifecycle": cases_lifecycle, "ui": cases_ui}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--only", choices=sorted(GROUPS), action="append")
    args = ap.parse_args()

    chosen = args.only or sorted(GROUPS)
    suite = Suite()
    started = time.time()

    for name in chosen:
        try:
            GROUPS[name].run(suite)
        except Exception as e:  # a crashed group is a failure, never a skip
            suite.record(f"{name} group crashed", name, False, f"{type(e).__name__}: {e}")

    elapsed = time.time() - started
    passed = len(suite.results) - len(suite.failed)
    print(f"\n{'=' * 60}")
    print(f"{passed}/{len(suite.results)} passed in {elapsed:.1f}s")

    if suite.failed:
        print("\nFAILED:")
        for r in suite.failed:
            print(f"  [{r.group}] {r.name}" + (f" — {r.detail}" if r.detail else ""))
            for line in r.evidence:
                print(f"      {line}")
        return 1

    print("all cases passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
