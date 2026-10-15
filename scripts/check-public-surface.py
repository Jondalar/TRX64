#!/usr/bin/env python3
"""Public-surface gate: no internal references where a user or an LLM can see them.

Owner rule: "No internal references on the public surface. File names ARE public
surface." Internal references are spec numbers, decision/section ids, internal bug
ids, issue numbers used as names. They may live in docs/internal/, in comments, in
commit messages and in test names. They may NOT appear in:

  1. `--help` of every trx64cli / trx64-daemon command and subcommand (the built
     binaries are run recursively);
  2. string literals of non-test code in crates/*/src (RPC answers, errors, notes,
     monitor output, panics) -- comments and #[cfg(test)] modules are stripped;
  3. package `description =` lines in Cargo.toml (published metadata);
  4. doc FILE NAMES and doc CONTENTS outside docs/internal/ (README.md, MONITOR.md,
     THANKS.md, CHECKLIST.md, docs/*.md);
  5. the values the trace store writes (covered by 2: they are string literals).

Exceptions live in scripts/public-surface-allow.txt, one reason per entry.

    scripts/check-public-surface.py            # all checks, builds the two binaries
    scripts/check-public-surface.py --no-help  # skip the --help walk (no build)
"""
import os, re, subprocess, sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
os.chdir(ROOT)

ID = re.compile(
    r"(?i:\bspecs?[ -]?\d)"          # Spec 853, spec-708, Specs 85x
    r"|\bBUG-?\d"                     # BUG-061
    r"|\bOQ\d|\bUX\d"                 # OQ4, UX1
    r"|/\d{3}-[a-z][\w.-]*\.md"        # a numbered spec file name used as a path
    r"|§"                             # section signs
    r"|(?<![\$\w])[DS][1-9]\b"        # decision / slice ids D7, S1 (not $D2)
    r"|(?:issue|PR) ?#\d+|(?<![&\w])#[1-9]\d{1,3}\b"  # issue numbers used as names
)
FILE_ID = re.compile(r"^(\d{3}[-_.]|spec[-_.]|bug[-_.]|BUG-)", re.I)

def load_allow():
    out = []
    p = os.path.join(ROOT, "scripts", "public-surface-allow.txt")
    for ln in open(p, encoding="utf-8"):
        ln = ln.rstrip("\n")
        if not ln.strip() or ln.lstrip().startswith("#"):
            continue
        if "|" not in ln:
            sys.exit("allow-list line without a reason: " + ln)
        pat, reason = [x.strip() for x in ln.split("|", 1)]
        if not reason:
            sys.exit("allow-list entry without a reason: " + ln)
        out.append((pat, reason))
    return out

ALLOW = load_allow()
def allowed(where, text):
    for pat, _ in ALLOW:
        if "::" in pat:
            w, t = [x.strip() for x in pat.split("::", 1)]
            if w in where and t in text:
                return True
        elif pat in where or pat in text:
            return True
    return False

fails = []
def hit(where, text):
    t = text.strip()
    if not allowed(where, t):
        fails.append("%s: %s" % (where, t[:160]))

# ---- 1. --help walk --------------------------------------------------------
def help_walk():
    tdir = os.environ.get("CARGO_TARGET_DIR", os.path.join(ROOT, "target"))
    r = subprocess.run(["cargo", "build", "--release", "-q", "-p", "trx64-cli", "-p", "trx64-daemon"])
    if r.returncode != 0:
        sys.exit("build of trx64-cli / trx64-daemon failed")
    for name in ("trx64cli", "trx64-daemon"):
        exe = os.path.join(tdir, "release", name)
        seen = set()
        def walk(path):
            key = tuple(path)
            if key in seen:
                return
            seen.add(key)
            p = subprocess.run([exe] + path + ["--help"], capture_output=True, text=True, timeout=60)
            txt = p.stdout + p.stderr
            where = "--help: " + " ".join([name] + path)
            for ln in txt.splitlines():
                if ID.search(ln):
                    hit(where, ln)
            sect = False
            for ln in txt.splitlines():
                if re.match(r"^[A-Za-z ]+:$", ln):
                    sect = ln.strip() == "Commands:"
                    continue
                if sect:
                    m = re.match(r"^  ([a-z0-9][\w-]*)\b", ln)
                    if m and m.group(1) != "help":
                        walk(path + [m.group(1)])
        walk([])
    return

# ---- 2. string literals in non-test code ----------------------------------
def strip_code(src):
    """Return src with comments blanked (newlines kept) and string literals kept."""
    out = []; i = 0; n = len(src)
    while i < n:
        c = src[i]
        if src.startswith("//", i):
            j = src.find("\n", i); j = n if j < 0 else j
            out.append(" " * (j - i)); i = j
        elif src.startswith("/*", i):
            depth = 1; j = i + 2
            while j < n and depth:
                if src.startswith("/*", j): depth += 1; j += 2
                elif src.startswith("*/", j): depth -= 1; j += 2
                else: j += 1
            out.append("".join(ch if ch == "\n" else " " for ch in src[i:j])); i = j
        elif c == "r" and re.match(r'r#*"', src[i:i+8]) and (i == 0 or not (src[i-1].isalnum() or src[i-1] == "_")):
            m = re.match(r'r(#*)"', src[i:]); h = m.group(1)
            end = src.find('"' + h, i + m.end())
            end = n if end < 0 else end + 1 + len(h)
            out.append(src[i:end]); i = end
        elif c == '"':
            j = i + 1
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            out.append(src[i:j+1]); i = j + 1
        elif c == "'":
            m = re.match(r"'(\\.[^']*|[^\\'])'", src[i:i+12])
            if m: out.append(m.group(0)); i += len(m.group(0))
            else: out.append(c); i += 1
        else:
            out.append(c); i += 1
    return "".join(out)

def drop_test_blocks(code):
    """Blank `#[cfg(test)] ... { ... }` items (mod tests, test fns) by brace matching."""
    res = list(code)
    for m in re.finditer(r"#\[cfg\(test\)\]", code):
        j = m.end(); depth = 0; started = False
        while j < len(code):
            ch = code[j]
            if ch == '"':
                k = j + 1
                while k < len(code) and code[k] != '"':
                    k += 2 if code[k] == "\\" else 1
                j = k
            elif ch == "{": depth += 1; started = True
            elif ch == "}":
                depth -= 1
                if started and depth == 0: break
            elif ch == ";" and not started: break
            j += 1
        for k in range(m.start(), min(j + 1, len(code))):
            if res[k] != "\n": res[k] = " "
    return "".join(res)

LIT = re.compile(r'r(#*)"(.*?)"\1|"((?:[^"\\]|\\.)*)"', re.S)
def scan_sources():
    for dp, dn, fn in os.walk(os.path.join(ROOT, "crates")):
        dn[:] = [d for d in dn if d not in ("tests", "benches", "examples", "target", "build")]
        if "/src" not in dp + "/":
            continue
        for f in fn:
            if not f.endswith(".rs"):
                continue
            path = os.path.join(dp, f)
            rel = os.path.relpath(path, ROOT)
            if f in ("tests.rs",) or f.endswith("_tests.rs"):
                continue
            code = drop_test_blocks(strip_code(open(path, encoding="utf-8").read()))
            for m in LIT.finditer(code):
                s = m.group(2) if m.group(2) is not None else m.group(3)
                if s and ID.search(s):
                    line = code.count("\n", 0, m.start()) + 1
                    hit("%s:%d" % (rel, line), s.replace("\\n", " ").replace("\n", " "))

# ---- 3. docs: file names + contents ---------------------------------------
USER_DOCS = ["README.md", "MONITOR.md", "THANKS.md", "CHECKLIST.md",
             "crates/trx64-cli/README.md", "crates/trx64-ffi/API.md"]
def scan_docs():
    files = subprocess.run(["git", "ls-files", "--cached", "--others", "--exclude-standard"],
                           capture_output=True, text=True).stdout.split("\n")
    for f in files:
        if not f or not os.path.exists(f):
            continue
        if f.startswith("docs/") and not f.startswith("docs/internal/"):
            if FILE_ID.match(os.path.basename(f)) or ID.search(f):
                hit("file name: " + f, f)
        if f.endswith("Cargo.toml"):
            for no, ln in enumerate(open(f, encoding="utf-8"), 1):
                if ln.startswith("description") and ID.search(ln):
                    hit("%s:%d" % (f, no), ln)
        user = f in USER_DOCS or (f.startswith("docs/") and not f.startswith("docs/internal/") and f.endswith(".md"))
        if user:
            for no, ln in enumerate(open(f, encoding="utf-8"), 1):
                if ID.search(ln):
                    hit("%s:%d" % (f, no), ln)
                for lm in re.finditer(r"\]\(([^)#]+)", ln):
                    if "internal/" in lm.group(1):
                        hit("%s:%d" % (f, no), "user doc links an internal doc: " + lm.group(1))

if "--no-help" not in sys.argv:
    help_walk()
scan_sources()
scan_docs()
if fails:
    print("PUBLIC SURFACE RED -- %d internal reference(s) where a user or LLM can see them:" % len(fails))
    for f in fails:
        print("  " + f)
    sys.exit(1)
print("public surface: clean (%d allow-list entries)" % len(ALLOW))
