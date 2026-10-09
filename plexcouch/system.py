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


def power_args(kind, exe, app_name):
    """Build the shutdown.exe argv. Pure: runs nothing.

    Shutdown uses /hybrid (Fast Startup, like Start > Shut down); Windows
    rejects /hybrid together with /r, so restart omits it.
    """
    flags = {"shutdown": ["/s", "/hybrid"], "restart": ["/r"]}[kind]
    return [exe] + flags + ["/t", "60", "/c", "%s: oprire in 60 de secunde" % app_name]


class Actions:
    def __init__(self, dry_run, app_name):
        self.dry_run = dry_run
        self.app_name = app_name
        self._power = None
        self._lock = threading.Lock()

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
        """kind: 'shutdown' | 'restart'. Schedules in 60s."""
        args = power_args(kind, _shutdown_exe(), self.app_name)
        with self._lock:
            if self.dry_run:
                log.info("dry-run: power %s (would run shutdown %s)", kind, " ".join(args[1:]))
            else:
                log.info("power %s scheduled", kind)
                r = subprocess.run(args, creationflags=CREATE_NO_WINDOW, capture_output=True, timeout=15)
                if r.returncode != 0:
                    log.warning("shutdown %s failed with code %s", kind, r.returncode)
                    return None
            self._power = {"kind": kind, "at": time.time() + 60}
            return logic.pending_power(self._power)

    def cancel_power(self):
        with self._lock:
            if self.dry_run:
                log.info("dry-run: power cancel (would run shutdown /a)")
            else:
                r = subprocess.run([_shutdown_exe(), "/a"], creationflags=CREATE_NO_WINDOW,
                                   capture_output=True, timeout=15)
                log.info("power cancel (code %s)", r.returncode)
            self._power = None
        return True

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
