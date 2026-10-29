"""Gate real relay startup before authorization, without timing assumptions."""

from __future__ import annotations

import importlib.util
import os
import select
import signal
import subprocess
import sys
import tempfile
import time
from importlib.machinery import SourceFileLoader
from pathlib import Path


def worker(script: str, events: int, gate: int, marker: str, mode: str) -> None:
    """Run the real wrapper with an invocation-owned relay startup barrier."""
    loader = SourceFileLoader("timeout_startup_fixture", script)
    spec = importlib.util.spec_from_loader(loader.name, loader)
    assert spec is not None
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    watchdog = module.stdio_relay_watchdog
    setsid = os.setsid

    def gated_watchdog(
        direction: str, source: int, destination: int, state: int, parent: int, chain: str
    ) -> None:
        """Hold actual startup while preserving the parent's cancellation pipe."""
        # The real watchdog owns a session before it can block. Retain the
        # parent's actual liveness channel so cancellation can release this
        # fixture just as it releases a blocked real relay.
        setsid()
        os.setsid = lambda: None
        with open(Path(marker).with_name("relays"), "a", encoding="ascii") as output:
            output.write(f"{os.getpid()}\n")
        os.write(events, b"I" if direction == "input" else b"G")
        while True:
            readable, _, _ = select.select([gate, parent], [], [])
            if parent in readable and os.read(parent, 1) == b"":
                os._exit(0)
            if gate in readable:
                os.read(gate, 1)
                break
        if mode in {"failed", "abort-unproved"}:
            module.publish_relay_state(state, b"X")
            os._exit(0)
        watchdog(direction, source, destination, state, parent, chain)

    module.stdio_relay_watchdog = gated_watchdog
    read_exact = module.read_exact_fd

    def observed_read(fd: int, size: int, deadline: float) -> bytes:
        """Tell the controller this worker is awaiting actual relay evidence."""
        if fd in module.stdio_relay_state_fds.values():
            pid = next(pid for pid, state in module.stdio_relay_state_fds.items() if state == fd)
            os.write(events, b"w" if module.stdio_relay_pids[pid] == "input" else b"W")
        return read_exact(fd, size, deadline)

    module.read_exact_fd = observed_read
    authorize = module.authorize_launch

    def observed_authorize(pid: int) -> bool:
        """Observe the real target admission boundary, optionally denying it."""
        os.write(events, b"A")
        return False if mode == "denied" else authorize(pid)

    module.authorize_launch = observed_authorize
    if mode == "abort-unproved":
        stop_relays = module.stop_stdio_relays

        def unproved_abort(deadline: float, drain_output: bool = True) -> bool:
            """Reap real workers, then inject an unproved abort receipt."""
            complete = stop_relays(deadline, drain_output)
            return complete if drain_output else False

        module.stop_stdio_relays = unproved_abort
    target = (
        "import os,sys; from pathlib import Path; "
        "Path(sys.argv[1]).write_text('executed'); "
        "os.write(1,sys.stdin.buffer.read()+b'bytes\\x00\\xff\\n\\n'); "
        "os.write(2,b'diagnostic\\n\\n')"
    )
    if mode == "input-cancelled":
        target = """
import signal, sys
from pathlib import Path
def stopped(signum, frame):
    Path(sys.argv[1]).with_name('target-term').write_text(str(signum))
    raise SystemExit(0)
signal.signal(signal.SIGTERM, stopped)
Path(sys.argv[1]).write_text('executed')
sys.stdin.buffer.read()
"""

    def open_fds() -> set[int]:
        """Compare live descriptor ownership even after an injected failure."""
        result = set()
        for directory in ("/proc/self/fd", "/dev/fd"):
            try:
                names = os.listdir(directory)
                break
            except OSError:
                continue
        else:
            raise AssertionError("no process descriptor inventory is available")
        for name in names:
            if not name.isdecimal() or int(name) < 3:
                continue
            fd = int(name)
            try:
                os.fstat(fd)
            except OSError:
                # The directory enumerator may have occupied a descriptor
                # that listdir closed before returning its complete listing.
                continue
            result.add(fd)
        return result

    if sys.platform.startswith("linux"):
        # Fedora's libffi retains its loaded-library descriptor on the first
        # ctypes import, independently of this supervisor. Initialize the
        # Linux subreaper's language runtime before measuring owned channels.
        import ctypes

        ctypes.CDLL(None)
    before = open_fds()
    status = module.main([script, "5", sys.executable, "-c", target, marker], terminal=True)
    after = open_fds()
    opened = {}
    for fd in after - before:
        try:
            opened[fd] = os.readlink(f"/proc/self/fd/{fd}")
        except OSError:
            opened[fd] = f"mode {os.fstat(fd).st_mode:o}"
    assert after == before, ("startup altered descriptors", opened, "closed", before - after)
    assert module.lease_reader is None and module.control_reader is None
    assert module.ancestor_registration is None
    module.terminal_exit(status)


def exercise(script: str, mode: str) -> None:
    """Authorize only after actual readiness; cancellation must never exec."""
    with tempfile.TemporaryDirectory(prefix="dot-relay-startup-") as temporary:
        marker = Path(temporary) / "executed"
        events, event_writer = os.pipe()
        gate, release = os.pipe()
        child = None
        try:
            child = subprocess.Popen(
                [
                    sys.executable,
                    __file__,
                    "--worker",
                    script,
                    str(event_writer),
                    str(gate),
                    str(marker),
                    mode,
                ],
                env={},
                pass_fds=(event_writer, gate),
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                start_new_session=True,
            )
            os.close(event_writer)
            event_writer = -1
            os.close(gate)
            gate = -1
            seen = set()
            deadline = time.monotonic() + 10
            while not {ord("G"), ord("W")} <= seen:
                remaining = deadline - time.monotonic()
                assert remaining > 0, ("startup barrier not reached", seen)
                readable, _, _ = select.select([events], [], [], remaining)
                assert readable, ("startup barrier timed out", seen)
                data = os.read(events, 64)
                assert data, ("worker exited before readiness wait", child.poll(), seen)
                seen.update(data)
                assert ord("A") not in seen, "target authorized before output relay readiness"
            assert not marker.exists(), "target ran while the output relay was gated"
            assert child.poll() is None, "wrapper stopped before startup was released"
            if mode == "expired":
                # Leave the startup gate closed. The existing absolute launch
                # protocol deadline, rather than a fixture sleep, ends it.
                pass
            elif mode == "input-cancelled":
                # Release just the two output workers. The input worker will
                # enter the same real startup gate only after target exec.
                os.write(release, b"xx")
                deadline = time.monotonic() + 10
                while not {ord("I"), ord("w")} <= seen:
                    remaining = deadline - time.monotonic()
                    assert remaining > 0, ("input startup barrier not reached", seen)
                    assert select.select([events], [], [], remaining)[0]
                    data = os.read(events, 64)
                    assert data, ("worker exited before input readiness", child.poll())
                    seen.update(data)
                while not marker.exists():
                    assert time.monotonic() < deadline, "target did not become ready"
                    time.sleep(0.01)
                child.send_signal(signal.SIGTERM)
            elif mode == "cancelled":
                child.send_signal(signal.SIGTERM)
            else:
                os.close(release)
                release = -1
            stdout, stderr = child.communicate(input=b"input\x00\n\n", timeout=12)
            if mode == "input-cancelled":
                assert child.returncode == 128 + signal.SIGTERM, (child.returncode, stderr)
                assert marker.with_name("target-term").read_text() == str(int(signal.SIGTERM))
                assert stdout == b"" and stderr == b"", (stdout, stderr)
            elif mode == "cancelled":
                assert child.returncode == 128 + signal.SIGTERM, (child.returncode, stderr)
                assert not marker.exists(), "cancelled startup executed the target"
                assert stdout == b"" and stderr == b"", (stdout, stderr)
            elif mode in {"failed", "denied", "expired"}:
                assert child.returncode == 126, (child.returncode, stderr)
                assert not marker.exists(), "failed startup executed the target"
                assert stdout == b"" and stderr == b"", (stdout, stderr)
            elif mode == "abort-unproved":
                assert child.returncode == 125, (child.returncode, stderr)
                assert not marker.exists(), "unproved abort executed the target"
                assert stdout == b"", stdout
                assert stderr == b"test-timeout-v1: could not verify child cleanup\n", stderr
            else:
                assert child.returncode == 0, (child.returncode, stderr)
                assert marker.read_text() == "executed"
                assert stdout == b"input\x00\n\nbytes\x00\xff\n\n", stdout
                assert stderr == b"diagnostic\n\n", stderr
            for pid in marker.with_name("relays").read_text().splitlines():
                ps = "/system/bin/ps" if Path("/system/bin/ps").is_file() else "/bin/ps"
                observed = subprocess.run(
                    [ps, "-p", pid, "-o", "stat="], capture_output=True, check=False
                ).stdout.strip()
                assert not observed or observed.startswith(b"Z"), ("relay survived", pid, observed)
        finally:
            if release >= 0:
                os.close(release)
            for fd in (events, event_writer, gate):
                if fd >= 0:
                    os.close(fd)
            if child is not None and child.poll() is None:
                child.send_signal(signal.SIGTERM)
                try:
                    child.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=5)


if __name__ == "__main__":
    if sys.argv[1] == "--worker":
        worker(sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), sys.argv[5], sys.argv[6])
    else:
        for mode in (
            "completed",
            "cancelled",
            "input-cancelled",
            "failed",
            "denied",
            "expired",
            "abort-unproved",
        ):
            exercise(sys.argv[1], mode)
        missing = subprocess.run(
            [sys.argv[1], "5", "/nonexistent/dot-relay-startup-fixture"],
            env={},
            capture_output=True,
            timeout=10,
        )
        assert missing.returncode == 127, (missing.returncode, missing.stderr)
        assert missing.stdout == b"" and missing.stderr == b"", missing
