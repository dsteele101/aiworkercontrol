# Drives the interactive `freebuff` TUI in a pseudo-terminal, because it has
# no headless mode. Run as: python3 -I -c <this file> <cwd> <idle-seconds>
# with the prompt on stdin. Prints one JSON object per line on stdout.
# With a third argument, `report`, it instead opens `/model` and prints the
# screen's lines that mention hours, which is where the free allowance shows.
import fcntl, json, os, pty, re, select, shutil, struct, sys, termios, time

cwd, idle = sys.argv[1], float(sys.argv[2])
report = len(sys.argv) > 3
prompt = "/model" if report else sys.stdin.read().strip()
# `freebuff` installs itself under ~/.config/manicode when run from npm.
binary = shutil.which("freebuff") or os.path.expanduser("~/.config/manicode/freebuff")


def emit(**event):
    print(json.dumps(event), flush=True)


if not os.access(binary, os.X_OK):
    emit(type="result", ok=False, summary="freebuff is not installed")
    sys.exit(1)

pid, fd = pty.fork()
if pid == 0:
    os.chdir(cwd)
    os.execv(binary, ["freebuff", "--cwd", cwd, "--trust-agents"])
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 160, 0, 0))


seen = b""


def drain(seconds):
    """Reads output for `seconds`; False if freebuff exited."""
    global seen
    end = time.time() + seconds
    while time.time() < end:
        if select.select([fd], [], [], 0.2)[0]:
            try:
                data = os.read(fd, 65536)
            except OSError:
                return False
            if not data:
                return False
            seen += data
    return True


def stop(ok, summary, quiet=False):
    try:
        os.kill(pid, 9)
        os.waitpid(pid, 0)
    except OSError:
        pass
    if not quiet:
        emit(type="result", ok=ok, summary=summary)
    sys.exit(0 if ok else 1)


# The TUI draws a banner first; it ignores input until then.
if not drain(8):
    stop(False, "freebuff exited before it started (is it logged in? run `freebuff login`)")
# Text, then Enter on its own: sent together the Enter is taken as a newline.
os.write(fd, prompt.encode())
time.sleep(1)
os.write(fd, b"\r")
if report:
    drain(4)
    text = re.sub(rb"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07]*\x07", b"\n", seen)
    for line in text.decode("utf8", "replace").splitlines():
        if "hours" in line:
            print(line.strip())
    stop(True, None, quiet=True)
emit(type="started")

# While working it redraws a spinner and elapsed time every second, so a
# quiet terminal means it is waiting for the next message: the task is done.
last = time.time()
while time.time() - last < idle:
    if select.select([fd], [], [], 0.5)[0]:
        try:
            if not os.read(fd, 65536):
                stop(False, "freebuff exited mid-run")
            last = time.time()
        except OSError:
            stop(False, "freebuff exited mid-run")
stop(True, None)
