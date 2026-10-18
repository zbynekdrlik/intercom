#!/usr/bin/env python3
"""Janus's TURN REST settings in /etc/janus/janus.jcfg (camera-box issue 1453).

A phone on mobile data reaches the intercom through the strih box's outbound tunnel, and its WebRTC
media needs a relay Janus can reach too. Janus asks the intercom hub for short-lived Cloudflare TURN
credentials through its own `nat.turn_rest_api` (the draft-uberti TURN REST API, a GET per
PeerConnection): the hub's `/api/turn-rest` on loopback. So the `nat:` section of the PACKAGE's
janus.jcfg gets two settings, inside one managed block:

    nat: {
        # camera-box issue 1453: TURN credentials from the intercom hub (setup-strih step 16i) -- managed, do not edit
        turn_rest_api = "http://127.0.0.1:8790/api/turn-rest"
        turn_rest_api_method = "GET"
        # end camera-box issue 1453
        ...the package's own lines, untouched...
    }

Only the managed block is ever written or removed; every other line of the file stays byte-identical.
A `turn_rest_api` / `turn_rest_api_method` the operator set by hand outside the block is a CONFLICT
(libconfig refuses a duplicate setting, so Janus would not start): exit 2, the file untouched.

    janus_turn_rest_cfg.py --file F --set URL   -> the block present with URL   (prints changed|unchanged)
    janus_turn_rest_cfg.py --file F --clear     -> the block absent             (prints changed|unchanged)
    janus_turn_rest_cfg.py --file F --check URL -> rc 0 iff the block is present with URL (prints ok|absent|differs)

The write is atomic (a temp file in the same directory, renamed over the file) and keeps the file's
mode and, as root, its owner. Exit 1 on a read/write error, 2 on a conflict or a malformed nat section.
"""
import argparse
import os
import re
import sys
import tempfile

BEGIN = ("# camera-box issue 1453: TURN credentials from the intercom hub (setup-strih step 16i)"
         " -- managed, do not edit")
END = "# end camera-box issue 1453"
NAT_OPEN = re.compile(r"^\s*nat\s*:\s*\{")
FOREIGN = re.compile(r"^\s*turn_rest_api(_method)?\s*=")


class JcfgError(Exception):
    """The file cannot be changed safely (a hand-set key, an unterminated section)."""


def _strip_block(lines):
    out, inside = [], False
    for ln in lines:
        s = ln.strip()
        if s == BEGIN:
            inside = True
            continue
        if inside and s == END:
            inside = False
            continue
        if not inside:
            out.append(ln)
    if inside:
        raise JcfgError("the managed block has no end marker -- fix the file by hand")
    return out


def _depth_change(line):
    """Braces on one libconfig line, outside a "string" and before a # / // comment."""
    depth, in_str, i = 0, False, 0
    while i < len(line):
        c = line[i]
        if in_str:
            if c == "\\":
                i += 1
            elif c == '"':
                in_str = False
        elif c == '"':
            in_str = True
        elif c == "#" or line.startswith("//", i):
            break
        elif c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
        i += 1
    return depth


def _nat_span(lines):
    """(open index, close index) of the nat section, or None when there is none."""
    for i, ln in enumerate(lines):
        if NAT_OPEN.match(ln):
            depth = _depth_change(ln)
            for j in range(i + 1, len(lines)):
                depth += _depth_change(lines[j])
                if depth <= 0:
                    return i, j
            raise JcfgError("the nat section is not closed")
    return None


def _block(url, indent):
    return [indent + BEGIN + "\n",
            '%sturn_rest_api = "%s"\n' % (indent, url),
            '%sturn_rest_api_method = "GET"\n' % indent,
            indent + END + "\n"]


def apply(text, url):
    """The new file text: the managed block present with `url`, or absent when `url` is None."""
    if url is not None and ('"' in url or "\n" in url or not url.startswith(("http://", "https://"))):
        raise JcfgError("not a usable TURN REST url: %r" % url)
    lines = _strip_block(text.splitlines(keepends=True))
    span = _nat_span(lines)
    if span is not None:
        for ln in lines[span[0] + 1:span[1]]:
            if FOREIGN.match(ln):
                raise JcfgError("nat already sets %s by hand -- remove it (this tool manages it)" % ln.strip())
    if url is None:
        return "".join(lines)
    if span is None:
        if lines and not lines[-1].endswith("\n"):
            lines[-1] += "\n"
        return "".join(lines + ["\n", "nat: {\n"] + _block(url, "\t") + ["}\n"])
    first = lines[span[0] + 1] if span[0] + 1 < span[1] else "\t"
    indent = first[:len(first) - len(first.lstrip())] or "\t"
    if indent.strip():
        indent = "\t"
    return "".join(lines[:span[0] + 1] + _block(url, indent) + lines[span[0] + 1:])


def check(text, url):
    """ok | absent | differs: whether the managed block is present inside nat with exactly `url`."""
    lines = text.splitlines()
    starts = [i for i, ln in enumerate(lines) if ln.strip() == BEGIN]
    if not starts:
        return "absent"
    i = starts[0]
    body = [ln.strip() for ln in lines[i + 1:i + 4]]
    want = ['turn_rest_api = "%s"' % url, 'turn_rest_api_method = "GET"', END]
    span = _nat_span(text.splitlines(keepends=True))
    inside = span is not None and span[0] < i < span[1]
    return "ok" if body == want and inside and len(starts) == 1 else "differs"


def _write(path, text):
    st = os.stat(path)
    d = os.path.dirname(os.path.abspath(path))
    fd, tmp = tempfile.mkstemp(prefix=".janus.jcfg.", dir=d)
    try:
        with os.fdopen(fd, "w") as f:
            f.write(text)
        os.chmod(tmp, st.st_mode & 0o7777)
        if os.geteuid() == 0:
            os.chown(tmp, st.st_uid, st.st_gid)
        os.replace(tmp, path)
    except BaseException:
        if os.path.exists(tmp):
            os.unlink(tmp)
        raise


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--file", required=True)
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("--set", metavar="URL")
    g.add_argument("--clear", action="store_true")
    g.add_argument("--check", metavar="URL")
    a = ap.parse_args(argv)
    try:
        with open(a.file) as f:
            text = f.read()
    except OSError as e:
        print("janus_turn_rest_cfg: cannot read %s: %s" % (a.file, e), file=sys.stderr)
        return 1
    try:
        if a.check is not None:
            verdict = check(text, a.check)
            print(verdict)
            return 0 if verdict == "ok" else 1
        new = apply(text, None if a.clear else a.set)
    except JcfgError as e:
        print("janus_turn_rest_cfg: %s: %s" % (a.file, e), file=sys.stderr)
        return 2
    if new == text:
        print("unchanged")
        return 0
    try:
        _write(a.file, new)
    except OSError as e:
        print("janus_turn_rest_cfg: cannot write %s: %s" % (a.file, e), file=sys.stderr)
        return 1
    print("changed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
