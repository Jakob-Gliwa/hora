#!/usr/bin/env python3
"""Rewrite a fork (0.11.x-fork) config for the port onto upstream 0.12.

Only pure renames the port does not accept under the fork's spelling:

- `expected_statuses = [...]` (a list of codes) becomes upstream's
  `expected_status`: a single code, or a list where every complete hundred
  (200..299) is written as its class ("2xx"). Note that a list accepting a
  3xx makes upstream judge the first answer (no redirect is followed),
  where the fork followed redirects unless every accepted code was one.

The file is rewritten line by line, so comments, order and `${VAR}`
references stay as they are. Values are never printed (the summary on
stderr counts changes per key), so the tool is safe on a private config.

Usage: port-config.py INPUT [-o OUTPUT]   (default: stdout)
"""

import argparse
import re
import sys
import tomllib

STATUSES = re.compile(r"^(?P<indent>\s*)expected_statuses(?P<eq>\s*=\s*)(?P<rest>.*)$")


def classes(codes):
    """The shortest upstream spelling of a set of status codes."""
    codes = set(codes)
    if any(not isinstance(code, int) or not 100 <= code <= 599 for code in codes):
        raise ValueError("expected_statuses holds something other than HTTP status codes")
    items = []  # (sort key, spelling)
    for hundred in range(100, 600, 100):
        block = set(range(hundred, hundred + 100))
        if block <= codes:
            items.append((hundred, f'"{hundred // 100}xx"'))
            codes -= block
    items.extend((code, str(code)) for code in codes)
    spellings = [spelling for _, spelling in sorted(items)]
    if len(spellings) == 1:
        return spellings[0]
    return "[" + ", ".join(spellings) + "]"


def split_comment(text):
    """The value part and a trailing comment of one TOML line (no strings
    with '#' occur in a status list)."""
    if "#" in text:
        value, comment = text.split("#", 1)
        return value.rstrip(), "  #" + comment
    return text.rstrip(), ""


def convert(text):
    """Return (converted text, {key: number of rewrites})."""
    lines = text.splitlines(keepends=True)
    out, changes, i = [], {}, 0
    while i < len(lines):
        line = lines[i]
        match = STATUSES.match(line.rstrip("\n"))
        if not match:
            out.append(line)
            i += 1
            continue
        # The array may span lines: gather until its brackets balance.
        value = match.group("rest")
        while value.count("[") > value.count("]") and i + 1 < len(lines):
            i += 1
            value += " " + lines[i].strip()
        value, comment = split_comment(value)
        codes = tomllib.loads("v = " + value)["v"]
        if isinstance(codes, int):
            codes = [codes]
        newline = "\n" if line.endswith("\n") else ""
        out.append(
            f"{match.group('indent')}expected_status{match.group('eq')}{classes(codes)}{comment}{newline}"
        )
        changes["expected_statuses -> expected_status"] = (
            changes.get("expected_statuses -> expected_status", 0) + 1
        )
        i += 1
    return "".join(out), changes


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("input")
    parser.add_argument("-o", "--output", help="write here instead of stdout")
    args = parser.parse_args(argv)
    with open(args.input, encoding="utf-8") as handle:
        text = handle.read()
    converted, changes = convert(text)
    # Still TOML, and the same document apart from the renames.
    tomllib.loads(converted)
    if args.output:
        with open(args.output, "w", encoding="utf-8") as handle:
            handle.write(converted)
    else:
        sys.stdout.write(converted)
    for key, count in sorted(changes.items()):
        print(f"{key}: {count}", file=sys.stderr)
    if not changes:
        print("nothing to rename", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
