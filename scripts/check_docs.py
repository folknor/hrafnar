#!/usr/bin/env python3
"""Enforce the documentation rules from AGENTS.md ("Document folders") and the
prose guide `docs/` and `reference/` are written to.

`brokkr check` runs this as the `doc-conventions` script check. It reads text
only and writes nothing. Every rule below is one a script can decide without
judgement; the guide's other rules (voice, sentence length, saying a fact once)
stay with the reviewer.

Rules, each reported with its name:

  history        `docs/` and `reference/` state the current contract, so no
                 "used to", "was removed", "previously", "older builds" and
                 their kin (`HISTORY_DOCS`). `CHANGELOG.md` is where history
                 goes and is not read at all. Code comments get a narrower
                 set from the `history-in-code-comments` textlint rule in
                 `brokkr.toml`, which tokenizes Rust and so reads comments
                 only: a user-facing error string that names a retired form
                 may say it was removed.
  hyphen         no hyphen used as punctuation (" - " between words) in
                 `docs/` or `reference/` prose. Use a colon, comma or period.
  docs-internal  `docs/` is what an installed binary prints, so it names no
                 `reference/` file, no source path (`src/...`) and no internal
                 Rust path (`Foo::bar`, `crate::`).
  link           every relative markdown link and `#anchor` in `docs/`,
                 `reference/`, `AGENTS.md` and `README.md` resolves, using
                 GitHub's heading slugs; and every `reference/<file>.md` or
                 `reference/<file>.md#anchor` cited from `src/` or `tests/`
                 resolves. Doc restructures leave stale citations behind, and
                 nothing else notices.
  notes          nothing durable cites `notes/`: not `docs/`, `reference/`,
                 `src/`, `tests/`, `scripts/` or the root config files. Code
                 is read whole, strings included, since a string outlives a
                 note as surely as a comment does.
  heading        headings render in a terminal: one H1 per file, no H4 or
                 deeper, no inline code, links or emphasis, no trailing
                 punctuation, at most 60 characters.
  fence          every code fence in `docs/` and `reference/` opens with a
                 language tag (`toml`, `text`, `sh`).
  word           the guide's banned words (easy, simple, simply, quick, just,
                 very, really) in `docs/` and `reference/` prose. Inline code
                 and fences are not read.
  wrap           `docs/` and `reference/` prose lines wrap at 78 columns.
                 Tables, fences, headings and lines carrying a URL are exempt.

What a prose rule reads: every line outside a fenced block, with inline code
spans blanked first, so a flag, a path or a TOML key quoted in backticks never
trips a word rule.

Escape hatch: a line containing `doc-ok` is skipped by every rule. Use it in a
comment, `<!-- doc-ok: reason -->`, for the rare line a rule misreads.

The last line printed is the sentinel brokkr matches: `doc conventions ok` on
success, a violation count otherwise.
"""

import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent

DOC_DIRS = ("docs", "reference")
LINKED_ROOT_FILES = ("AGENTS.md", "README.md")
CODE_DIRS = ("src", "tests")
MARKER = "doc-ok"

# "used to" as past habit, not the passive "is used to check". Lowercase only,
# so a sentence-initial "Used to assert ..." (a purpose clause) is not read.
_USED_TO = r"(?<!\bis )(?<!\bare )(?<!\bbe )(?<!\bbeen )(?<!\bbeing )(?<!\bnever )(?<!\bonly )(?<!\balso )(?<!\bnot )\bused to\b"
# Deliberately absent, because in this tree they mostly describe the present:
#   "no longer"  a runtime state change ("once it retries, interrupt no longer
#                targets the failed attempt")
#   "legacy"     codex's own name for its `sandbox_mode` pipeline, and the
#                on-disk spellings `review` still reads
#   "deprecated" a current state a doc may need to state
HISTORY_DOCS = re.compile(
    _USED_TO
    + r"|\b(?i:previously|formerly|historical(?:ly)?|older (?:builds?|versions?)"
    r"|(?:was|were|has been|have been) (?:removed|renamed|replaced)|this changed)\b"
)
# A hyphen with a space either side, between words or ending a wrapped line.
# A list marker is stripped before matching, so `- item` is never read.
HYPHEN = re.compile(r"\S - \S|\S -$")
BANNED = re.compile(r"\b(?:easy|easily|simple|simply|quick|quickly|just|very|really)\b", re.IGNORECASE)
REFERENCE_PATH = re.compile(r"\breference/")
SOURCE_PATH = re.compile(r"(?<![\w/.-])(?:src|tests)/[\w./-]+")
RUST_PATH = re.compile(r"\bcrate::|\b[A-Z][A-Za-z0-9]*::[a-z_][A-Za-z0-9_]*")
FENCE = re.compile(r"^(\s*)(`{3,}|~{3,})(.*)$")
HEADING = re.compile(r"^(#{1,6})\s+(.*?)\s*#*\s*$")
LINK = re.compile(r"(?<!!)\[(?:[^\]\\]|\\.)*\]\(([^)\s]+)(?:\s+\"[^\"]*\")?\)")
CODE_REFERENCE_CITATION = re.compile(r"(?<![\w/.-])((?:reference|docs)/[\w.-]+\.md)(#[\w-]+)?")
COMMENT = re.compile(r"^\s*//")
# A path into `notes/`. The bare folder name is not a citation: a durable
# file may say what `notes/` is for.
NOTES_CITATION = re.compile(r"(?<![\w/.-])notes/[\w.-][\w./-]*")
# Durable root files a `notes/` citation is read in. AGENTS.md, CLAUDE.md and
# README.md are left out: they describe the whole layout, `notes/` included.
NOTES_ROOT_FILES = ("brokkr.toml", "Cargo.toml", ".review.toml")
# This file spells `notes/` to describe the rule.
SELF = "scripts/check_docs.py"
URL = re.compile(r"https?://")
WRAP = 78
HEADING_MAX = 60


def tracked() -> list[str]:
    out = subprocess.run(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    )
    return sorted(name for name in out.stdout.split("\0") if name and (ROOT / name).is_file())


def blank_code_spans(line: str) -> str:
    """Replace each inline code span's content with spaces, keeping columns."""
    return re.sub(r"(`+)(.+?)\1", lambda m: m.group(1) + " " * len(m.group(2)) + m.group(1), line)


def markdown_lines(text: str):
    """Yield (number, raw line, in_fence, fence_open_info) for a markdown file.

    `fence_open_info` is the info string of an opening fence line, else None.
    """
    fence = None
    for number, line in enumerate(text.splitlines(), start=1):
        match = FENCE.match(line)
        if fence is None and match:
            fence = match.group(2)[0] * len(match.group(2))
            yield number, line, True, match.group(3).strip()
            continue
        if fence is not None:
            if match and match.group(2).startswith(fence) and not match.group(3).strip():
                fence = None
            yield number, line, True, None
            continue
        yield number, line, False, None


def slug(heading: str) -> str:
    """GitHub's anchor for a heading: markup dropped, lowercased, punctuation
    other than `-` and `_` removed, each space a hyphen."""
    text = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", heading)
    text = text.replace("`", "").replace("*", "")
    text = text.strip().lower()
    text = re.sub(r"[^\w\- ]", "", text)
    return text.replace(" ", "-")


_anchor_cache: dict[pathlib.Path, set[str]] = {}


def text_anchors(text: str) -> set[str]:
    """Every heading anchor in `text`; a repeated slug gets `-1`, `-2`."""
    seen: dict[str, int] = {}
    found = set()
    for _, line, in_fence, _ in markdown_lines(text):
        if in_fence:
            continue
        match = HEADING.match(line)
        if not match:
            continue
        base = slug(match.group(2))
        count = seen.get(base, 0)
        seen[base] = count + 1
        found.add(base if count == 0 else "%s-%d" % (base, count))
    return found


def anchors(path: pathlib.Path) -> set[str]:
    if path not in _anchor_cache:
        _anchor_cache[path] = text_anchors(path.read_text())
    return _anchor_cache[path]


def resolve_link(source: pathlib.Path, target: str) -> str | None:
    """Why `target`, written in `source`, does not resolve; None if it does."""
    if re.match(r"^[a-z][a-z0-9+.-]*:", target, re.IGNORECASE):
        return None
    file_part, _, anchor = target.partition("#")
    dest = source if not file_part else (source.parent / file_part).resolve()
    if not dest.exists():
        return "%s does not exist" % file_part
    if anchor and dest.suffix == ".md" and anchor not in anchors(dest):
        return "no heading with anchor #%s in %s" % (anchor, dest.relative_to(ROOT))
    return None


def check_markdown(rel: str, durable: bool, violations: list[str], text: str | None = None) -> None:
    path = ROOT / rel
    in_docs = rel.startswith("docs/")
    h1 = 0
    text = path.read_text() if text is None else text
    own_anchors = text_anchors(text)
    for number, line, in_fence, info in markdown_lines(text):
        where = "%s:%d" % (rel, number)
        if MARKER in line:
            continue
        if in_fence:
            if durable and info is not None and not info:
                violations.append("%s: fence: a code fence needs a language tag (toml, text, sh)" % where)
            continue

        for target in LINK.findall(line):
            if target.startswith("#"):
                problem = None if target[1:] in own_anchors else "no heading with anchor %s here" % target
            else:
                problem = resolve_link(path, target)
            if problem:
                violations.append("%s: link: %s" % (where, problem))
        if not durable:
            continue

        prose = blank_code_spans(line)
        heading = HEADING.match(line)
        table = line.lstrip().startswith("|")

        if heading:
            level, text = len(heading.group(1)), heading.group(2)
            if level == 1:
                h1 += 1
                if h1 == 2:
                    violations.append("%s: heading: a second H1; H1 is the page title only" % where)
            if level >= 4:
                violations.append("%s: heading: H4 or deeper; use a bold list lead instead" % where)
            if "`" in text or "](" in text or re.search(r"\*\w|\w\*|(?<!\w)_\w|\w_(?!\w)", text):
                violations.append("%s: heading: inline code, a link or emphasis; keep headings plain text" % where)
            if re.search(r"[.:;,!?]$", text):
                violations.append("%s: heading: trailing punctuation" % where)
            if len(text) > HEADING_MAX:
                violations.append("%s: heading: %d characters, over %d" % (where, len(text), HEADING_MAX))

        match = HISTORY_DOCS.search(prose)
        if match:
            violations.append(
                "%s: history: %r; state the current contract, history goes in CHANGELOG.md"
                % (where, match.group(0))
            )
        if not table and HYPHEN.search(re.sub(r"^\s*[-*+] ", "  ", prose)):
            violations.append("%s: hyphen: a hyphen as punctuation; use a colon, comma or period" % where)
        word = BANNED.search(prose)
        if word:
            violations.append("%s: word: %r; cut it or say what it means" % (where, word.group(0)))
        if in_docs:
            for pattern, what in (
                (REFERENCE_PATH, "a reference/ path"),
                (SOURCE_PATH, "a source path"),
                (RUST_PATH, "an internal Rust path"),
            ):
                if pattern.search(line):
                    violations.append(
                        "%s: docs-internal: %s; an installed binary has no checkout to follow it into"
                        % (where, what)
                    )
        if not heading and not table and not URL.search(line) and len(line) > WRAP:
            violations.append("%s: wrap: %d columns, over %d" % (where, len(line), WRAP))
    if durable and h1 == 0:
        violations.append("%s:1: heading: no H1 page title" % rel)


def check_code(rel: str, violations: list[str], text: str | None = None) -> None:
    text = (ROOT / rel).read_text() if text is None else text
    for number, line in enumerate(text.splitlines(), start=1):
        if not COMMENT.match(line) or MARKER in line:
            continue
        where = "%s:%d" % (rel, number)
        for file_part, anchor in CODE_REFERENCE_CITATION.findall(line):
            problem = resolve_link(ROOT / "README.md", file_part + anchor)
            if problem:
                violations.append("%s: link: cites %s%s, but %s" % (where, file_part, anchor, problem))


# In-memory cases run before the tree is read: each rule must fire on a line
# written to break it and stay quiet on its look-alike, or no tree result is
# reported. A rule whose regex rots would otherwise pass every tree silently.
SELF_TEST = [
    ("docs/t.md", "# T\n\nIt used to fail.\n", {"history"}),
    ("docs/t.md", "# T\n\nThis is used to check it.\n", set()),
    ("docs/t.md", "# T\n\nThe flag was removed.\n", {"history"}),
    ("docs/t.md", "# T\n\nThe `provider` key, `` `x` was removed ``.\n", set()),
    ("docs/t.md", "# T\n\nA run - then a stop.\n", {"hyphen"}),
    ("docs/t.md", "# T\n\n- a list item\n  - nested\n", set()),
    ("docs/t.md", "# T\n\nSee reference/codex.md for more.\n", {"docs-internal"}),
    ("docs/t.md", "# T\n\nIn src/main.rs.\n", {"docs-internal"}),
    ("docs/t.md", "# T\n\nCalls `Foo::bar` here.\n", {"docs-internal"}),
    ("reference/t.md", "# T\n\nCalls `Foo::bar` here.\n", set()),
    ("reference/t.md", "# T\n\n[x](#nowhere)\n", {"link"}),
    ("reference/t.md", "# T\n\n## The run's end\n\n[x](#the-runs-end)\n", set()),
    ("reference/t.md", "# T\n\n## A\n\n## A\n\n[x](#a-1)\n", set()),
    ("reference/t.md", "# T\n\n[x](missing.md)\n", {"link"}),
    ("reference/t.md", "# T\n\n# Two\n", {"heading"}),
    ("reference/t.md", "# T\n\n#### Deep\n", {"heading"}),
    ("reference/t.md", "# T\n\n## The `key`\n", {"heading"}),
    ("reference/t.md", "# T\n\n## Ends in a colon:\n", {"heading"}),
    ("reference/t.md", "# T\n\n## " + "x" * 61 + "\n", {"heading"}),
    ("reference/t.md", "# T\n\n## snake_case_word\n", set()),
    ("reference/t.md", "# T\n\n```\nx\n```\n", {"fence"}),
    ("reference/t.md", "# T\n\n```text\nit used to - just\n```\n", set()),
    ("reference/t.md", "# T\n\nIt is just here.\n", {"word"}),
    ("reference/t.md", "# T\n\nThe `just` flag.\n", set()),
    ("reference/t.md", "# T\n\n" + "word " * 16 + "\n", {"wrap"}),
    ("reference/t.md", "# T\n\n| " + "cell " * 20 + "|\n", set()),
    ("reference/t.md", "# T\n\nIt used to - just <!-- doc-ok -->\n", set()),
    ("AGENTS.md", "# T\n\nIt used to - just\n", set()),
    ("reference/t.md", "# T\n\nThe plan in `notes/plan.md`.\n", {"notes"}),
    ("src/t.rs", "/// See `reference/codex.md#nowhere`.\n", {"link"}),
    ("src/t.rs", "/// See `reference/nothing.md`.\n", {"link"}),
    ("src/t.rs", "// The plan is in notes/plan.md.\n", {"notes"}),
    ("scripts/t.py", "# from notes/x.md\n", {"notes"}),
    ("scripts/t.py", "# from my_notes/x.md\n", set()),
    ("scripts/t.py", "# nothing durable cites `notes/`\n", set()),
]


def self_test() -> list[str]:
    failures = []
    for rel, text, expected in SELF_TEST:
        found: list[str] = []
        if rel.endswith(".py"):
            check_notes(rel, found, text)
        elif rel.endswith(".rs"):
            check_code(rel, found, text)
            check_notes(rel, found, text)
        else:
            durable = rel.split("/")[0] in DOC_DIRS
            check_markdown(rel, durable, found, text)
            if durable:
                check_notes(rel, found, text)
        rules = {v.split(": ")[1] for v in found}
        if rules != expected:
            failures.append("self-test: %s %r: expected %s, got %s" % (rel, text, sorted(expected), found))
    return failures


def check_notes(rel: str, violations: list[str], text: str | None = None) -> None:
    text = (ROOT / rel).read_text() if text is None else text
    for number, line in enumerate(text.splitlines(), start=1):
        if MARKER in line:
            continue
        for match in NOTES_CITATION.finditer(line):
            violations.append(
                "%s:%d: notes: cites %s; notes/ carries no truth guarantee, so carry the context here"
                % (rel, number, match.group(0))
            )


def main() -> int:
    failures = self_test()
    if failures:
        for failure in failures:
            print(failure)
        print("doc convention self-test failed; no tree result reported")
        return 1
    files = tracked()
    durable = [f for f in files if f.endswith(".md") and f.split("/")[0] in DOC_DIRS]
    if not any(f.startswith("docs/") for f in durable) or not any(f.startswith("reference/") for f in durable):
        print("docs/ or reference/ holds no markdown; a pass over nothing is not a pass")
        return 1
    code = [f for f in files if f.endswith(".rs") and f.split("/")[0] in CODE_DIRS]
    violations: list[str] = []
    for rel in durable:
        check_markdown(rel, True, violations)
    for rel in LINKED_ROOT_FILES:
        if (ROOT / rel).is_file():
            check_markdown(rel, False, violations)
    for rel in code:
        check_code(rel, violations)
    for rel in files:
        top = rel.split("/")[0]
        if rel != SELF and (top in DOC_DIRS + CODE_DIRS + ("scripts",) or rel in NOTES_ROOT_FILES):
            check_notes(rel, violations)
    for violation in violations:
        print(violation)
    if violations:
        print("%d doc convention violation(s)" % len(violations))
        return 1
    print("checked %d document(s) and %d source file(s)" % (len(durable), len(code)))
    print("doc conventions ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
