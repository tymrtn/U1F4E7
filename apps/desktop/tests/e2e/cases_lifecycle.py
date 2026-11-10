"""App lifecycle: what the shell spawns, and what it leaves behind."""

from __future__ import annotations

import contextlib
import os
import subprocess
import time

from harness import APP_BIN, BUNDLE, Suite, throwaway_home


def children(pid: int) -> list[str]:
    out = subprocess.run(["pgrep", "-P", str(pid)], capture_output=True, text=True).stdout.split()
    cmds = []
    for child in out:
        c = subprocess.run(["ps", "-o", "command=", "-p", child], capture_output=True, text=True).stdout.strip()
        if c:
            cmds.append(c)
    return cmds


@contextlib.contextmanager
def app(home, env_extra: dict | None = None):
    env = dict(os.environ)
    env["HOME"] = str(home)
    env.pop("ENVELOPE_DESKTOP_BIN", None)
    if env_extra:
        env.update(env_extra)
    proc = subprocess.Popen([str(APP_BIN)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        yield proc
    finally:
        if proc.poll() is None:
            proc.terminate()
            with contextlib.suppress(subprocess.TimeoutExpired):
                proc.wait(timeout=5)
        if proc.poll() is None:
            proc.kill()
        # Never let a test leak a server holding a database.
        subprocess.run(["pkill", "-f", str(BUNDLE / "Contents" / "MacOS" / "envelope") + " serve"], capture_output=True)


def wait_for_child(proc, seconds=30.0):
    deadline = time.time() + seconds
    while time.time() < deadline:
        c = children(proc.pid)
        if c:
            return c
        if proc.poll() is not None:
            return []
        time.sleep(0.05)
    return []


def run(suite: Suite) -> None:
    group = "lifecycle"
    print("\n== app lifecycle ==", flush=True)

    if not APP_BIN.is_file():
        suite.record("bundle exists", group, False, f"missing {APP_BIN}; run `npm run tauri build`")
        return

    # The shipped app must run the server it carries, not one from PATH, since a
    # Finder launch has neither the repo nor a useful PATH.
    with throwaway_home() as home:
        with app(home) as proc:
            found = wait_for_child(proc)
            from_bundle = any(str(BUNDLE) in c for c in found)
            suite.record(
                "spawns the server bundled inside the app",
                group,
                bool(found) and from_bundle,
                evidence=found[:1] or ["(no child spawned)"],
            )
            # Sweeps must stay on, or queued mail is never transmitted.
            correct_flags = any(
                "--no-auth" in c and "--no-background-sweeps" not in c for c in found
            )
            suite.record(
                "spawns it unauthenticated and with sweeps running",
                group,
                correct_flags,
                detail="--no-background-sweeps would silently strand every send",
                evidence=found[:1] or ["(no child)"],
            )

    # An unresolvable binary must surface a diagnostic, not crash and not
    # silently start something else.
    with throwaway_home() as home:
        with app(home, {"ENVELOPE_DESKTOP_BIN": "/nonexistent/envelope"}) as proc:
            time.sleep(6)
            alive = proc.poll() is None
            spawned = children(proc.pid)
            suite.record(
                "survives an unresolvable server binary",
                group,
                alive and not spawned,
                evidence=[f"app alive={alive} children={len(spawned)}"],
            )

    # Quitting must not orphan the server. Drop does not run under Tauri's
    # process::exit, so this is covered by an explicit exit handler.
    leaked = []
    for cycle in range(3):
        with throwaway_home() as home:
            with app(home) as proc:
                wait_for_child(proc)
                subprocess.run(["osascript", "-e", 'quit app "Envelope"'], capture_output=True)
                deadline = time.time() + 15
                while time.time() < deadline and proc.poll() is None:
                    time.sleep(0.2)
                time.sleep(2)
                survivors = subprocess.run(
                    ["pgrep", "-f", str(BUNDLE / "Contents" / "MacOS" / "envelope") + " serve"],
                    capture_output=True,
                    text=True,
                ).stdout.split()
                if survivors:
                    leaked.append(f"cycle {cycle + 1}: {len(survivors)} left running")

    suite.record(
        "three launch/quit cycles leave no server behind",
        group,
        not leaked,
        evidence=leaked or ["0 orphaned servers across 3 cycles"],
    )
