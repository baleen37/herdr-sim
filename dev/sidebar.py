#!/usr/bin/env -S uv run --quiet --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["pyte>=0.8"]
# ///
"""Print the Herdr sidebar as the running server draws it, for checking a
plugin change without looking at the screen.

Attaches one more client to the running session in a pseudo-terminal, reads
what it draws for a moment, then kills that client. Herdr gives each client
its own view, so the user's own window keeps its workspace and tab; the
server and its panes are untouched.

    dev/sidebar.py            # the sidebar's text
    dev/sidebar.py --color    # each run of text with its colour, e.g. [#d97757]
"""

import argparse
import fcntl
import os
import pty
import select
import signal
import struct
import termios
import time

import pyte


class Screen(pyte.Screen):
    """Answers the client's terminal queries the way a terminal would, so it
    does not sit waiting on them; pyte's own handlers trip on private ones."""

    def __init__(self, cols: int, rows: int, fd: int):
        super().__init__(cols, rows)
        self.fd = fd

    def write_process_input(self, data: str) -> None:
        os.write(self.fd, data.encode())

    def report_device_status(self, mode: int, **_) -> None:
        if mode == 6:
            self.write_process_input(f"\x1b[{self.cursor.y + 1};{self.cursor.x + 1}R")

    def report_device_attributes(self, mode: int = 0, **kwargs) -> None:
        if not kwargs.get("private"):
            self.write_process_input("\x1b[?62;22c")


def capture(cols: int, rows: int, seconds: float) -> pyte.Screen:
    pid, fd = pty.fork()
    if pid == 0:
        os.environ.update(TERM="xterm-256color", COLORTERM="truecolor")
        os.execvp("herdr", ["herdr"])
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    screen = Screen(cols, rows, fd)
    stream = pyte.ByteStream(screen)
    end = time.monotonic() + seconds
    try:
        while (left := end - time.monotonic()) > 0:
            ready, _, _ = select.select([fd], [], [], left)
            if not ready:
                continue
            try:
                data = os.read(fd, 65536)
            except OSError:
                break
            if not data:
                break
            stream.feed(data)
    finally:
        os.kill(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
        os.close(fd)
    return screen


def colour(char) -> str:
    fg = char.fg
    if fg == "default":
        return ""
    return f"#{fg}" if len(fg) == 6 and all(c in "0123456789abcdef" for c in fg.lower()) else fg


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--width", type=int, default=40, help="sidebar columns to print")
    parser.add_argument("--cols", type=int, default=160)
    parser.add_argument("--rows", type=int, default=50)
    parser.add_argument("--seconds", type=float, default=2.5)
    parser.add_argument("--color", action="store_true")
    args = parser.parse_args()
    screen = capture(args.cols, args.rows, args.seconds)
    for y in range(args.rows):
        line = screen.buffer[y]
        cells = [line[x] for x in range(args.width)]
        if not args.color:
            print("".join(c.data for c in cells).rstrip())
            continue
        out, current = [], None
        for c in cells:
            style = colour(c) + (" bold" if c.bold else "")
            if c.data.strip() and style != current:
                out.append(f"[{style or 'default'}]")
                current = style
            out.append(c.data)
        print("".join(out).rstrip())


if __name__ == "__main__":
    main()
