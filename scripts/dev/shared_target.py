#!/usr/bin/env python3
"""Share a canonical target and hold its maintenance lock until a command exits."""

import os
from pathlib import Path
import re
import signal
import subprocess
import sys


def cargo_target_args(args):
    """Resolve Cargo's explicit target option, leaving test/program arguments alone."""
    target = None
    index = 0
    while index < len(args) and args[index] != "--":
        arg = args[index]
        if arg in ("--target-dir", "--config"):
            index += 1
            if index == len(args) or not args[index] or args[index] == "--":
                raise ValueError(f"{arg} needs a value")
            value = args[index]
        elif arg.startswith(("--target-dir=", "--config=")):
            arg, value = arg.split("=", 1)
        elif arg == "--build-dir" or arg.startswith("--build-dir="):
            raise ValueError("separate --build-dir is unsupported; use --target-dir")
        else:
            index += 1
            continue
        if arg == "--target-dir":
            if not value or target is not None:
                raise ValueError("provide one nonempty --target-dir")
            target = value
        else:
            # Support ordinary one-line overrides without implementing a second
            # TOML parser. Quoted/escaped keys and config files can hide output
            # overrides; build/alias overrides can change the locked directory.
            match = re.fullmatch(r"\s*([A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)*)\s*=(.*)", value)
            if (not match or "\n" in value or "\r" in value
                    or match[1].split(".")[0] in ("build", "alias")):
                raise ValueError(
                    "unsupported --config override: use a simple non-build/non-alias "
                    "key=value, CARGO_TARGET_DIR/--target-dir, or repository configuration"
                )
        index += 1
    return target


def target_path(repo, explicit=None):
    target = explicit or os.environ.get("CARGO_TARGET_DIR")
    if not target:
        common = Path(subprocess.check_output(
            ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
            cwd=repo, text=True,
        ).strip())
        if common.name != ".git" or not (common.parent / "Cargo.toml").is_file():
            raise ValueError("Set CARGO_TARGET_DIR explicitly for a checkout with a separate Git directory.")
        target = common.parent / "target"
    # Both entrypoints run from the checkout root, including relative overrides.
    return (repo / target).resolve()


def supervised(command, repo, env):
    """Keep the lock in this parent even if host-build closes inherited fds."""
    child = None
    interrupted = None
    try:
        terminal = os.open("/dev/tty", os.O_RDWR)
    except OSError:
        has_terminal = False
    else:
        os.close(terminal)
        has_terminal = True

    def forward(signum, _frame):
        nonlocal interrupted
        interrupted = signum
        if child is not None:
            try:
                if not has_terminal:
                    os.killpg(child.pid, signum)
                elif signum != signal.SIGINT:
                    # Terminal INT already reaches the foreground group. Keep
                    # that group/session so interactive commands retain /dev/tty.
                    child.send_signal(signum)
            except ProcessLookupError:
                pass

    for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, forward)
    child = subprocess.Popen(command, cwd=repo, env=env, start_new_session=not has_terminal)
    if interrupted is not None:
        forward(interrupted, None)
    status = child.wait()
    if interrupted is not None:
        return 128 + interrupted
    return status if status >= 0 else 128 - status


def main():
    try:
        import fcntl
    except ImportError:
        raise ValueError("shared target locking requires Python 3 on macOS or Linux") from None

    mode, raw_repo, launcher, *args = sys.argv[1:]
    repo = Path(raw_repo).resolve()
    if os.environ.get("CARGO_BUILD_BUILD_DIR"):
        raise ValueError("separate CARGO_BUILD_BUILD_DIR is unsupported; use CARGO_TARGET_DIR")
    target = target_path(repo, cargo_target_args(args) if mode == "cargo" else None)
    if mode == "cargo":
        target.mkdir(parents=True, exist_ok=True)
        command = ([launcher, "--", "cargo"] if launcher else ["cargo"]) + args
    elif mode == "sweep":
        if not target.is_dir():
            raise ValueError(f"target dir {target} does not exist")
        profiles = [args[index + 1] for index, arg in enumerate(args[:-1]) if arg == "--profile"]
        for name in profiles or ["debug"]:
            if target not in (target / name).resolve().parents:
                raise ValueError("--profile must resolve to a directory inside the target")
        command = [launcher, "-c", 'source "$1"; shift; target_sweep_main "$@"',
                   "target-sweep", str(repo / "scripts/dev/target_sweep.sh"), *args]
    else:
        raise ValueError(f"unknown shared target operation: {mode}")
    env = dict(os.environ, CARGO_TARGET_DIR=str(target))
    # This is deliberately not Cargo's own lock, which Cargo must acquire itself.
    # Keep the file in place: unlinking it can create two independent locks.
    with (target / ".zeroclaw-maintenance.lock").open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | (fcntl.LOCK_NB if mode == "sweep" else 0))
        except BlockingIOError:
            print(f"error: target is busy; refusing sweep under {target}", file=sys.stderr)
            return 75
        return supervised(command, repo, env)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        # Includes cancellation while waiting for the target lock, before any child.
        sys.exit(130)
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"error: {error}", file=sys.stderr)
        sys.exit(2)
