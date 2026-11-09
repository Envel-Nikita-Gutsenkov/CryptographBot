#!/usr/bin/env python3
"""Compact extractor for Telegram HTML chat exports.

Usage:
    python3 scripts/extract_dialogs.py messages4.html [messages5.html ...] > dialogs.txt
    python3 scripts/extract_dialogs.py --compact messages4.html ...
    python3 scripts/extract_dialogs.py --scan messages4.html ...

Standard library only (no bs4/lxml). Prints every message as:

    [YYYY-MM-DD HH:MM:SS] Author #id
    <flattened text; rich-message tables/lists become plain text>

Deliberately compact so the whole conversation can be grepped for symptoms
(timeouts, duplicated menus, contradictions, raw markup, ...).

`--scan` instead prints a compact audit: suspicious formatting patterns
(double signs, negative zero, raw markup, raw provider errors, unknown
commands) plus bot messages repeated within a short window. Each finding is
reported with its date so it can be checked against the deploy that fixed it.
"""
import re
import sys
from html.parser import HTMLParser


def date_from_title(title):
    # "03.09.2026 13:30:41 UTC+03:00" -> "2026-09-03 13:30:41"
    m = re.match(r"(\d{2})\.(\d{2})\.(\d{4}) (\d{2}:\d{2}:\d{2})", title or "")
    if not m:
        return title or ""
    return f"{m.group(3)}-{m.group(2)}-{m.group(1)} {m.group(4)}"


class DialogParser(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        # each frame: {"tag", "classes", "role"} where role in
        # {None, "msg", "text", "author", "service"}
        self.stack = []
        self.messages = []   # (date, author, id, text)
        self.separators = [] # (message_index, label)

        self.msg_date = ""
        self.msg_author = ""
        self.msg_id = ""
        self.msg_buf = []
        self.author_buf = []
        self.svc_buf = []
        self.last_author = ""  # Telegram groups "joined" messages under one author

    # -- stack helpers ---------------------------------------------------------
    def _role_on_top(self):
        for frame in reversed(self.stack):
            if frame["role"]:
                return frame["role"]
        return None

    def _flush(self):
        if not self.msg_id:
            return
        if not self.msg_author:
            self.msg_author = self.last_author  # "joined" continuation
        if self.msg_author:
            self.last_author = self.msg_author
        text = re.sub(r"[ \t]+\n", "\n", "".join(self.msg_buf)).strip()
        text = re.sub(r"\n{3,}", "\n\n", text)
        self.messages.append((self.msg_date, self.msg_author, self.msg_id, text))
        self.msg_buf = []
        self.msg_id = ""  # prevent a second flush of the same message

    # -- parser hooks ----------------------------------------------------------
    def handle_starttag(self, tag, attrs):
        classes = []
        a = {}
        for k, v in attrs:
            if k == "class" and v:
                classes = v.split()
            else:
                a[k] = v

        role = None
        if tag == "div" and "message" in classes and "default" in classes:
            self._flush()  # safety if previous was not closed
            self.msg_date = ""
            self.msg_author = ""
            self.msg_id = a.get("id", "").replace("message", "")
            self.msg_buf = []
            role = "msg"
        elif tag == "div" and "message" in classes and "service" in classes:
            self._flush()
            self.svc_buf = []
            role = "service"
        elif tag == "div" and "from_name" in classes and self._role_on_top() == "msg":
            self.author_buf = []
            role = "author"
        elif tag == "div" and "text" in classes and self._role_on_top() == "msg":
            if self.msg_buf and not self.msg_buf[-1].endswith("\n"):
                self.msg_buf.append("\n")
            role = "text"
        elif tag == "div" and "date" in classes and "details" in classes:
            self.msg_date = date_from_title(a.get("title", ""))

        self.stack.append({"tag": tag, "classes": classes, "role": role})

        # text-shaping only applies inside a text block
        if self._role_on_top() == "text":
            if tag == "br":
                self.msg_buf.append("\n")
            elif tag == "td":
                self.msg_buf.append(" | ")
            elif tag == "tr":
                self.msg_buf.append("\n")
            elif tag in ("p", "li", "div"):
                if self.msg_buf and not self.msg_buf[-1].endswith("\n"):
                    self.msg_buf.append("\n")
            elif tag in ("b", "strong"):
                pass
            elif tag == "code":
                self.msg_buf.append("`")
        elif self._role_on_top() == "author" and tag == "br":
            self.author_buf.append(" ")

    def handle_endtag(self, tag):
        if not self.stack:
            return
        if tag == "code" and self._role_on_top() == "text":
            self.msg_buf.append("`")
        # pop until we pop a matching tag (defensive against stray tags)
        while self.stack:
            frame = self.stack.pop()
            role = frame["role"]
            if role == "text":
                # trailing newline when the text block closes
                if self.msg_buf and not self.msg_buf[-1].endswith("\n"):
                    self.msg_buf.append("\n")
            elif role == "author":
                self.msg_author = "".join(self.author_buf).strip()
            elif role == "service":
                self.separators.append(
                    (len(self.messages), "".join(self.svc_buf).strip())
                )
            elif role == "msg":
                self._flush()
            if frame["tag"] == tag:
                break

    def handle_data(self, data):
        role = self._role_on_top()
        if role == "text":
            self.msg_buf.append(data)
        elif role == "author":
            self.author_buf.append(data)
        elif role == "service":
            self.svc_buf.append(data)


# Patterns that have historically shown up as real bugs in this bot's output.
# Each entry: (label, compiled regex). Keep them cheap and precise.
SUSPECT_PATTERNS = [
    ("double-sign", re.compile(r"[+\-]\s?[+\-]\s?\$?\d|\+\-\d")),
    ("dollar-before-minus", re.compile(r"\$-\d")),
    ("negative-zero", re.compile(r"-0\.0+(?:\D|$)")),
    ("nan-or-inf", re.compile(r"\b(?:NaN|inf|Infinity)\b", re.IGNORECASE)),
    ("raw-entity", re.compile(r"&(?:lt|gt|amp|quot|#\d+);")),
    # Backticks are omitted: the extractor itself renders <code> as `…`, so a
    # backtick is not evidence of a markdown leak.
    ("raw-markdown", re.compile(r"\*\*|__|\*\w[^*\n]*\*")),
    ("raw-provider-error", re.compile(
        r"(?:Error fetching|Error rendering|Failed to fetch)\s*:", re.IGNORECASE
    )),
    ("unknown-command", re.compile(r"Unknown command", re.IGNORECASE)),
    ("html-leak", re.compile(r"</?(?:b|i|code|pre|a)\b[^>]*>", re.IGNORECASE)),
]

# Bot message bodies that repeat this close together are almost always a
# double-send / re-fire race.
DUP_WINDOW_SECONDS = 180


def _epoch(date):
    """'YYYY-MM-DD HH:MM:SS' -> sortable string, or '' when unknown."""
    return date[:19] if re.match(r"\d{4}-\d{2}-\d{2}", date or "") else ""


def _seconds_between(a, b):
    from datetime import datetime
    try:
        da = datetime.strptime(a[:19], "%Y-%m-%d %H:%M:%S")
        db = datetime.strptime(b[:19], "%Y-%m-%d %H:%M:%S")
        return abs((db - da).total_seconds())
    except Exception:
        return None


def scan(paths):
    """Print a compact, reproducible audit of suspicious output."""
    total_msgs = 0
    findings = {label: [] for label, _ in SUSPECT_PATTERNS}
    duplicates = []

    for path in paths:
        p = DialogParser()
        with open(path, encoding="utf-8", errors="replace") as fh:
            p.feed(fh.read())
        p._flush()
        total_msgs += len(p.messages)

        prev_bot = None  # (date, normalised body, message index)
        for i, (date, author, mid, text) in enumerate(p.messages):
            flat = " ".join(text.split())
            is_bot = bool(author) and author.lower().startswith("cryptograph")

            if is_bot:
                for label, rx in SUSPECT_PATTERNS:
                    m = rx.search(flat)
                    if m:
                        findings[label].append(
                            (date, mid, flat[max(0, m.start() - 30): m.end() + 30])
                        )

                body = flat[:220]
                if prev_bot is not None:
                    pdate, pbody, pmid = prev_bot
                    gap = _seconds_between(pdate, date)
                    if body and pbody == body and gap is not None and gap <= DUP_WINDOW_SECONDS:
                        duplicates.append((date, mid, pmid, int(gap), body[:80]))
                prev_bot = (date, body, mid)
            else:
                prev_bot = None

    print(f"Scanned {total_msgs} messages from {len(paths)} export(s).\n")

    any_hit = False
    for label, _ in SUSPECT_PATTERNS:
        hits = findings[label]
        if not hits:
            continue
        any_hit = True
        print(f"[{label}] {len(hits)} hit(s):")
        for date, mid, ctx in hits[:12]:
            print(f"  {date} #{mid}: …{ctx}…")
        if len(hits) > 12:
            print(f"  … and {len(hits) - 12} more")
        print()

    if duplicates:
        any_hit = True
        print(f"[duplicate-bot-message] {len(duplicates)} hit(s) "
              f"(≤{DUP_WINDOW_SECONDS}s apart):")
        for date, mid, pmid, gap, body in duplicates[:12]:
            print(f"  {date} #{mid} repeats #{pmid} after {gap}s: {body}")
        if len(duplicates) > 12:
            print(f"  … and {len(duplicates) - 12} more")
        print()

    if not any_hit:
        print("No suspicious patterns found.")


def dump(paths, compact=False):
    for path in paths:
        p = DialogParser()
        with open(path, encoding="utf-8", errors="replace") as fh:
            p.feed(fh.read())
        p._flush()
        print(f"\n===== {path} =====")
        seps = dict(p.separators)
        for i, (date, author, mid, text) in enumerate(p.messages):
            if i in seps:
                print(f"\n----- {seps[i]} -----")
            if compact:
                # One line per message so a 2.5k-message export can be scanned.
                flat = " ".join(text.split())
                if len(flat) > 160:
                    flat = flat[:157] + "..."
                print(f"[{date[5:16]}] {author or '?'}: {flat}")
                continue
            print(f"[{date}] {author or '?'} #{mid}")
            if text:
                print(text)


if __name__ == "__main__":
    args = sys.argv[1:]
    compact = "--compact" in args
    scan_mode = "--scan" in args
    paths = [a for a in args if a not in ("--compact", "--scan")]
    if not paths:
        sys.exit(__doc__)
    if scan_mode:
        scan(paths)
    else:
        dump(paths, compact=compact)
