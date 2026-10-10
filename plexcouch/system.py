"""Windows actions (monitors, power) and PC info. Every action honours dry-run."""

import ctypes
import logging
import os
import shutil
import socket
import subprocess
import sys
import threading
import time

from . import logic

log = logging.getLogger("plexcouch")

HWND_BROADCAST = 0xFFFF
WM_SYSCOMMAND = 0x0112
SC_MONITORPOWER = 0xF170
MONITOR_OFF = 2
INPUT_MOUSE = 0
MOUSEEVENTF_MOVE = 0x0001
ES_DISPLAY_REQUIRED = 0x00000002
CREATE_NO_WINDOW = 0x08000000


def _shutdown_exe():
    root = os.environ.get("SystemRoot", r"C:\Windows")
    exe = os.path.join(root, "System32", "shutdown.exe")
    return exe if os.path.exists(exe) else "shutdown"


POWER_DELAY = 60


def power_args(kind, exe):
    """Build the shutdown.exe argv for an immediate shutdown/restart. Pure: runs nothing.

    Behaves like Start > Shut down: /hybrid keeps Fast Startup (Windows rejects it
    with /r), and /t 0 avoids the implied /f that any /t above 0 adds, so apps
    with unsaved work can still stop the shutdown. The 60 s delay is our own timer.
    """
    flags = {"shutdown": ["/s", "/hybrid"], "restart": ["/r"]}[kind]
    return [exe] + flags + ["/t", "0"]


def abort_args(exe):
    """shutdown.exe argv that aborts a shutdown/restart Windows already started. Pure."""
    return [exe, "/a"]


# cancel_power() outcomes
CANCELLED = "cancelled"  # the countdown was stopped before shutdown.exe ran
ABORTED = "aborted"      # shutdown.exe had already run; 'shutdown /a' stopped it
TOO_LATE = "too_late"    # shutdown.exe had already run and 'shutdown /a' failed
NOTHING = "nothing"      # nothing was pending


class Actions:
    def __init__(self, dry_run, app_name):
        self.dry_run = dry_run
        self.app_name = app_name
        self._power = None
        self._timer = None
        self._fired = None  # the scheduled action whose shutdown.exe ran (or is running)
        self._lock = threading.Lock()
        # Held while shutdown.exe runs. _fire takes it before it releases _lock, so a cancel
        # that sees the fired state waits for the command to finish before running /a.
        self._exec_lock = threading.Lock()

    # -- monitors ---------------------------------------------------------
    def monitors_off(self, reason="manual"):
        if self.dry_run:
            log.info("dry-run: monitors off (%s)", reason)
            return True
        log.info("monitors off (%s)", reason)
        # PostMessage, not SendMessage: SendMessage to HWND_BROADCAST can hang.
        ok = ctypes.windll.user32.PostMessageW(HWND_BROADCAST, WM_SYSCOMMAND, SC_MONITORPOWER, MONITOR_OFF)
        return bool(ok)

    def monitors_on(self):
        if self.dry_run:
            log.info("dry-run: monitors on")
            return True
        log.info("monitors on")
        sent = _send_mouse_nudge()
        ctypes.windll.kernel32.SetThreadExecutionState(ES_DISPLAY_REQUIRED)
        return sent == 2

    # -- power ------------------------------------------------------------
    def power(self, kind):
        """kind: 'shutdown' | 'restart'. Runs it after POWER_DELAY seconds unless cancelled."""
        args = power_args(kind, _shutdown_exe())
        with self._lock:
            if logic.pending_power(self._power) is not None:
                return None  # one at a time
            self._power = {"kind": kind, "at": time.time() + POWER_DELAY}
            self._fired = None
            self._timer = threading.Timer(POWER_DELAY, self._fire, (self._power, args))
            self._timer.daemon = True
            self._timer.start()
            log.info("%spower %s scheduled in %ss", "dry-run: " if self.dry_run else "", kind, POWER_DELAY)
            return logic.pending_power(self._power)

    def _fire(self, scheduled, args):
        # The decision to run is taken under _lock, and _exec_lock is taken before _lock is
        # released: a cancel either stops it here, or sees it fired and waits to run /a after.
        with self._lock:
            if self._power is not scheduled:
                return  # cancelled or replaced
            self._timer = None
            self._fired = scheduled
            self._exec_lock.acquire()
        try:
            kind = scheduled["kind"]
            if self.dry_run:
                log.info("dry-run: power %s (would run shutdown %s)", kind, " ".join(args[1:]))
                return
            log.info("power %s now", kind)
            try:
                r = subprocess.run(args, creationflags=CREATE_NO_WINDOW, capture_output=True, timeout=15)
                code = r.returncode
            except (OSError, subprocess.SubprocessError) as e:
                log.warning("shutdown %s failed: %s", kind, type(e).__name__)
                code = -1
            if code != 0:
                log.warning("shutdown %s failed with code %s", kind, code)
                with self._lock:
                    if self._power is scheduled:
                        self._power = None
                    if self._fired is scheduled:
                        self._fired = None
        finally:
            self._exec_lock.release()

    def cancel_power(self):
        """Cancel the pending shutdown/restart. Returns the outcome: CANCELLED (stopped before
        shutdown.exe ran), ABORTED or TOO_LATE (it had run; 'shutdown /a' worked or not) or
        NOTHING."""
        with self._lock:
            fired = self._fired
            pending = self._power is not None
            if self._timer is not None:
                self._timer.cancel()
                self._timer = None
            self._power = None
            self._fired = None
        if fired is None:
            log.info("%spower cancelled", "dry-run: " if self.dry_run else "")
            return CANCELLED if pending else NOTHING
        with self._exec_lock:  # wait for a shutdown.exe that is still running
            return self._abort(fired["kind"])

    def _abort(self, kind):
        args = abort_args(_shutdown_exe())
        if self.dry_run:
            log.info("dry-run: power %s already started, would run shutdown /a", kind)
            return ABORTED
        try:
            r = subprocess.run(args, creationflags=CREATE_NO_WINDOW, capture_output=True, timeout=15)
            code = r.returncode
        except (OSError, subprocess.SubprocessError) as e:
            log.warning("shutdown /a failed: %s", type(e).__name__)
            code = -1
        if code == 0:
            log.info("power %s aborted with shutdown /a", kind)
            return ABORTED
        log.warning("power %s could not be aborted (shutdown /a code %s)", kind, code)
        return TOO_LATE

    def pending(self):
        with self._lock:
            p = logic.pending_power(self._power)
            if p is None:
                self._power = None
            return p


# -- SendInput --------------------------------------------------------------

class _MOUSEINPUT(ctypes.Structure):
    _fields_ = [("dx", ctypes.c_long), ("dy", ctypes.c_long), ("mouseData", ctypes.c_ulong),
                ("dwFlags", ctypes.c_ulong), ("time", ctypes.c_ulong),
                ("dwExtraInfo", ctypes.c_size_t)]


class _KEYBDINPUT(ctypes.Structure):
    _fields_ = [("wVk", ctypes.c_ushort), ("wScan", ctypes.c_ushort), ("dwFlags", ctypes.c_ulong),
                ("time", ctypes.c_ulong), ("dwExtraInfo", ctypes.c_size_t)]


class _HARDWAREINPUT(ctypes.Structure):
    _fields_ = [("uMsg", ctypes.c_ulong), ("wParamL", ctypes.c_ushort), ("wParamH", ctypes.c_ushort)]


class _INPUTUNION(ctypes.Union):
    _fields_ = [("mi", _MOUSEINPUT), ("ki", _KEYBDINPUT), ("hi", _HARDWAREINPUT)]


class _INPUT(ctypes.Structure):
    _fields_ = [("type", ctypes.c_ulong), ("u", _INPUTUNION)]


def _send_mouse_nudge():
    """1-pixel relative mouse move and back; wakes the displays."""
    arr = (_INPUT * 2)()
    for i, dx in enumerate((1, -1)):
        arr[i].type = INPUT_MOUSE
        arr[i].u.mi = _MOUSEINPUT(dx, 0, 0, MOUSEEVENTF_MOVE, 0, 0)
    return ctypes.windll.user32.SendInput(2, ctypes.byref(arr), ctypes.sizeof(_INPUT))


# -- PC info ------------------------------------------------------------------

def uptime_seconds():
    try:
        f = ctypes.windll.kernel32.GetTickCount64
        f.restype = ctypes.c_ulonglong
        return int(f() / 1000)
    except Exception:
        return None


def disks(letters):
    out = []
    for d in letters:
        root = d if d.endswith("\\") else d + "\\"
        try:
            u = shutil.disk_usage(root)
        except OSError:
            continue
        out.append({"drive": d.rstrip("\\"), "free_gb": round(u.free / 1024 ** 3, 1),
                    "total_gb": round(u.total / 1024 ** 3, 1),
                    "used_pct": round((u.total - u.free) / u.total * 100, 1) if u.total else 0})
    return out


def free_gb(path):
    """Free space in GB on the drive holding path, or None if it cannot be read."""
    root = path if path.endswith("\\") else path + "\\"
    try:
        return shutil.disk_usage(root).free / 1024 ** 3
    except OSError:
        return None


def own_ipv4s():
    ips = set()
    try:
        for info in socket.getaddrinfo(socket.gethostname(), None, socket.AF_INET):
            ips.add(info[4][0])
    except OSError:
        pass
    try:  # primary outbound interface (no packet is sent for UDP connect)
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        try:
            s.connect(("10.255.255.255", 1))
            ips.add(s.getsockname()[0])
        finally:
            s.close()
    except OSError:
        pass
    return ips


def pc_info(letters):
    return {"hostname": socket.gethostname(), "uptime": uptime_seconds(), "disks": disks(letters),
            "platform": sys.platform}
