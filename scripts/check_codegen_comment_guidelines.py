#!/usr/bin/env python3
"""Check Comment Guidelines over Rust sources.

Detects:
- end-of-line // on code lines
- decorative banners (ASCII + Unicode box-drawing: ─ — ━ ═ etc.)
- process/changelog past-tense
- change narration ("Removed the fallback", "we changed ...")
- commented-out code blocks

Usage: check_codegen_comment_guidelines.py [path ...]   (default: crates/)
"""
from __future__ import annotations
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

# Unambiguous source-history narration: these phrases can only be talking about
# an edit, never about runtime state.
FORBID = re.compile(
    r"\b(previously|formerly|now handles|this code now|used to be|"
    r"in an earlier version|before this change|as of this (change|commit))\b",
    re.I,
)

# Passive edit verbs. "the flag was removed in v2" is narration; "entries that
# were removed are purged later" is runtime prose. No pattern separates the two,
# so these are reported for review and never fail the run — an earlier version
# failed on them, and the resulting synonym substitutions made comments wrong.
REVIEW = re.compile(
    r"\b((was|were|has been|have been) (added|removed|changed|updated|increased))\b",
    re.I,
)
# Change narration, not the English words. "cells changed", "removed entries
# are purged", "O(1) removal" all describe runtime behaviour and must pass; an
# earlier version of this pattern matched the bare verbs anywhere and drove a
# round of substitutions that made comments wrong.
BARE = re.compile(
    r"(?:^|(?<=[.;:!?]\s))\s*"
    r"(added|removed|changed|updated|renamed|introduced|reverted)"
    r"\s+(the|this|that|these|those|it|its|a|an|our|all|back|support|handling)\b"
    r"|\bwe\s+(added|removed|changed|updated|renamed|reverted)\b",
    re.I,
)

# ASCII + common Unicode box-drawing / heavy / double horizontal rules
BOX_CLASS = r"=\-─—━═_*－―"
BOX_CHARS = set("=─—━═_*－―⎯╔╗╚╝╠╣╦╩╬║│┃")


def is_banner_comment(s: str) -> bool:
    """True for // decorative section banners (not /// or //! docs)."""
    if not s.startswith("//") or s.startswith("///") or s.startswith("//!"):
        return False
    inn = s[2:].strip()
    if not inn:
        return False
    if re.fullmatch(rf"[{BOX_CLASS}\s]{{4,}}", inn):
        return True
    deco = sum(1 for ch in inn if ch in BOX_CHARS or ch in "=-_*")
    if deco >= 4 and re.match(rf"^[{BOX_CLASS}\s]", inn) and re.search(
        rf"[{BOX_CLASS}\s]$", inn
    ):
        return True
    if re.match(rf"^[{BOX_CLASS}]{{2,}}", inn) and re.search(rf"[{BOX_CLASS}]{{2,}}$", inn):
        return True
    return False


RAW_START = re.compile(r'(?:b|c)?r(#*)"')


def scan_comments(src: str):
    """Yield (line_no, text, kind, has_code_before) for every real comment.

    Line-based scans misfire on `//` inside a multi-line string (a C test
    fixture, a `postgres://` URL in help text). This walks the whole source
    with string/char/raw-string awareness so only genuine comments are judged.
    `kind` is 'line' (`//`), 'doc' (`///`/`//!`), or 'block' (`/* */`).
    """
    i, n, line_no = 0, len(src), 1
    line_start = 0

    def code_before(pos):
        return bool(src[line_start:pos].strip())

    while i < n:
        c = src[i]
        if c == "\n":
            line_no += 1
            i += 1
            line_start = i
            continue

        if c == "/" and i + 1 < n and src[i + 1] == "/":
            j = src.find("\n", i)
            j = n if j < 0 else j
            body = src[i:j]
            kind = "doc" if body[:3] in ("///", "//!") else "line"
            yield line_no, body, kind, code_before(i)
            i = j
            continue

        if c == "/" and i + 1 < n and src[i + 1] == "*":
            start_line, depth, i = line_no, 1, i + 2
            buf = []
            while i < n and depth:
                if src.startswith("/*", i):
                    depth, i = depth + 1, i + 2
                elif src.startswith("*/", i):
                    depth, i = depth - 1, i + 2
                else:
                    if src[i] == "\n":
                        line_no += 1
                        line_start = i + 1
                    buf.append(src[i])
                    i += 1
            yield start_line, "/*" + "".join(buf), "block", False
            continue

        m = RAW_START.match(src, i)
        if m:
            close = '"' + m.group(1)
            k = src.find(close, m.end())
            k = n if k < 0 else k + len(close)
            line_no += src.count("\n", i, k)
            nl = src.rfind("\n", i, k)
            if nl >= 0:
                line_start = nl + 1
            i = k
            continue

        if c == '"' or (c in "bc" and i + 1 < n and src[i + 1] == '"'):
            i += 1 if c == '"' else 2
            while i < n:
                if src[i] == "\\":
                    i += 2
                elif src[i] == '"':
                    i += 1
                    break
                elif src[i] == "\n":
                    line_no += 1
                    line_start = i + 1
                    i += 1
                else:
                    i += 1
            continue

        if c == "'":
            m = CHAR_LIT.match(src, i)
            if m:
                i = m.end()
                continue

        i += 1


CHAR_LIT = re.compile(r"'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]{1,6}\}|.)|[^\\'])'")


def main() -> int:
    eol = ban = forbid = bare = dead = review = 0
    files = 0
    samples: list[str] = []
    notes: list[str] = []
    roots = [Path(a).resolve() for a in sys.argv[1:]] or [ROOT / "crates"]
    sources = sorted(
        {p for r in roots for p in ([r] if r.suffix == ".rs" else r.rglob("*.rs"))}
    )
    for p in sources:
        if "target" in p.parts or "third_party" in p.parts:
            continue
        files += 1
        rel = p.relative_to(ROOT)
        prev_comment_body = ""
        for i, raw, kind, has_code_before in scan_comments(
            p.read_text(encoding="utf-8", errors="replace")
        ):
            s = raw.strip()
            if kind == "line" and has_code_before and "http://" not in s and "https://" not in s:
                if not re.search(r"function hello|TODO: implement", s):
                    eol += 1
                    if len(samples) < 20:
                        samples.append(f"EOL {rel}:{i}: {s[:100]}")
            if kind == "block":
                prev_comment_body = ""
                continue
            # Strip the marker first: the `!` of `//!` otherwise reads as
            # sentence-final punctuation to BARE's start-of-sentence lookbehind.
            body = re.sub(r"^//[/!]?", "", s)
            body = re.sub(r"`[^`]*`", "", body)
            body = re.sub(r"https?://\S+", "", body).strip()
            if FORBID.search(body):
                forbid += 1
                if len(samples) < 40:
                    samples.append(f"FORBID {rel}:{i}: {s[:100]}")
            if REVIEW.search(body):
                review += 1
                if len(notes) < 40:
                    notes.append(f"REVIEW {rel}:{i}: {s[:100]}")
            # A wrapped sentence continues onto this line, so its first word is
            # mid-sentence and the start-anchored half of BARE cannot apply.
            continuation = bool(prev_comment_body) and not re.search(
                r"[.:;!?]$|^\s*[-*|]|\|$", prev_comment_body
            )
            hit = BARE.search(body)
            if hit and not (continuation and hit.start() == 0):
                bare += 1
                if len(samples) < 40:
                    samples.append(f"BARE {rel}:{i}: {s[:100]}")
            prev_comment_body = body
            if is_banner_comment(s):
                ban += 1
                if len(samples) < 40:
                    samples.append(f"BANNER {rel}:{i}: {s[:100]}")
            if kind == "line":
                inn = s[2:].strip()
                # Require an identifier after the keyword so prose like
                # "type (alphabetical) then ..." is not mistaken for a
                # commented-out `type` alias.
                if re.match(
                    r"^((let|use|fn|pub|struct|enum|impl|const|type|trait)\s+\w|#\[)",
                    inn,
                ) and re.search(r"[;{}=]", inn):
                    dead += 1
                    if len(samples) < 40:
                        samples.append(f"DEAD {rel}:{i}: {s[:100]}")

    print(f"scanned_rs_files={files}")
    print(f"eol={eol} banners={ban} forbid={forbid} bare={bare} dead={dead}")
    print(f"review_only={review} (not a failure — judge each by hand)")
    for s in samples:
        print(s)
    for s in notes:
        print(s)
    ok = eol == ban == forbid == bare == dead == 0
    print("PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
