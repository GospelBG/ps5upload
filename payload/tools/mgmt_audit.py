#!/usr/bin/env python3
"""Static audits of the AVA1 management table (payload/src/mgmt_table.def).

  mgmt_audit.py table    every entry names a real AVA1_METHOD_* constant, a handler defined in
                         runtime.c, a method listed in protocol/ava1/MGMT_METHODS.md, once
  mgmt_audit.py recv     no management handler reads from client_fd after the header
                         (the capture path calls handlers with fd = -1)
  mgmt_audit.py sony     every entry whose handler can reach register/profile/registry/Remote
                         Play/notification code or a Sony API carries MGMT_SONY
  mgmt_audit.py stack    no stack array of 16 KiB or more is reachable from a table handler
                         (AVA1 workers have 512 KiB, the rule is the SPEC.md section 7.3 one)
  mgmt_audit.py report   every array of 2 KiB or more reachable from each handler in
                         MGMT_METHODS.md (the audit table of the Task 2 report)

  mgmt_audit.py selftest the analysis on synthetic C (struct, 2-D, pointer and function-pointer cases)

Exit status 0 = clean.

THIS IS A TRIPWIRE, NOT A PROOF. It reads C text, it does not compile it:
  * the call graph is by name over every payload/src/*.c function (comments and strings stripped); a
    function whose name appears as an argument counts as called (the `hw_info_get_text` pattern), so it
    over-approximates, but a call through a table, a struct member or a dlsym'd pointer is invisible;
  * array sizes are literals or #define expressions; `sizeof(...)` and unresolvable macros are skipped
    (listed as unknown by `report`), element types are scalars, typedefs of scalars, or structs whose
    members it can sum (a LOWER bound: no padding, a member it cannot size counts 8 bytes), and any other
    element type counts 8 bytes each, so an array it prices at 16 KiB can only be bigger;
  * alloca, VLAs, recursion depth and the stack the Sony libraries use are not seen.
A finding that is not real goes in ALLOW below with the reason; a clean run is evidence, not a guarantee.
"""
import glob
import os
import re
import sys

ROOT = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))
os.chdir(ROOT)

# (handler, function, array) -> reason. Keep empty unless a finding is a false positive.
ALLOW = {}
LIMIT = 16 * 1024
REPORT_MIN = 2048


def strip(b):
    def r(m):
        t = m.group(0)
        if t.startswith("/"):
            return " "
        return '""' if t[0] == '"' else "''"

    return re.sub(r"/\*.*?\*/|//[^\n]*|\"(?:\\.|[^\"\\\n])*\"|'(?:\\.|[^'\\\n])*'", r, b, flags=re.S)


DEFS = {"PATH_MAX": "1024"}
for f in glob.glob("src/*.c") + glob.glob("src/*.inc") + glob.glob("include/*.h") + glob.glob("ava1/*.h"):
    for l in open(f, errors="ignore"):
        m = re.match(r"\s*#\s*define\s+(\w+)\s+(.+)", l)
        if m:
            DEFS.setdefault(m.group(1), strip(m.group(2)).strip())


def ev(e, depth=0):
    if depth > 8:
        return None
    e = re.sub(r"\b(\d+)[uUlL]+\b", r"\1", e)
    e = re.sub(r"\(\s*(size_t|unsigned|int|uint\d+_t)\s*\)", "", e)
    if "sizeof" in e:
        return None

    def rep(m):
        n = m.group(0)
        if n in DEFS:
            v = ev(DEFS[n], depth + 1)
            return "(%d)" % v if v is not None else "None"
        return "None"

    e2 = re.sub(r"[A-Za-z_]\w*", rep, e)
    if "None" in e2 or not re.fullmatch(r"[\d\s+\-*()/]*", e2):
        return None
    try:
        return int(eval(e2))  # digits and + - * / ( ) only, checked above
    except Exception:
        return None


FUNCS = {}
for f in sorted(glob.glob("src/*.c")):
    cur, buf, hdr = None, [], False
    for l in open(f, errors="ignore"):
        if cur is None:
            if l and l[0] not in " \t/*#}\n" and "(" in l and not l.startswith(("typedef", "struct", "enum", "union", "extern")):
                m = re.search(r"(\w+)\s*\(", l)
                if m:
                    cur, buf, hdr = m.group(1), [l], "{" not in l
                    if l.rstrip().endswith(";"):
                        cur = None
        else:
            buf.append(l)
            if hdr:
                if "{" in l:
                    hdr = False
                elif l.rstrip().endswith(";"):
                    cur = None
                    continue
            if l.startswith("}") and not hdr:
                FUNCS.setdefault(cur, ("".join(buf), f))
                cur = None

SCALARS = {"char": 1, "uint8_t": 1, "int8_t": 1, "bool": 1, "_Bool": 1, "short": 2, "uint16_t": 2, "int16_t": 2, "int": 4, "unsigned": 4,
           "uint32_t": 4, "int32_t": 4, "float": 4, "long": 8, "uint64_t": 8, "int64_t": 8, "size_t": 8, "ssize_t": 8,
           "double": 8, "off_t": 8, "time_t": 8, "pid_t": 4, "uintptr_t": 8, "intptr_t": 8, "pthread_mutex_t": 64, "pthread_t": 8}
TYPEDEFS = {}  # name -> bytes (scalar aliases and structs whose members can be summed)
STRUCTS = {}   # tag -> bytes
UNKNOWN_ELEM = 8


def elem_size(tp, depth=0):
    tp = " ".join(tp.split())
    if depth > 6:
        return UNKNOWN_ELEM
    tp = re.sub(r"^(const|volatile|static|unsigned|signed)\s+", lambda m: "" if m.group(1) != "unsigned" else "unsigned ", tp)
    if tp.endswith("*"):
        return 8
    if tp.startswith("struct "):
        return STRUCTS.get(tp[7:], UNKNOWN_ELEM)
    if tp.startswith("unsigned "):
        rest = tp[9:].strip() or "int"
        return SCALARS.get(rest, 4 if rest == "int" else UNKNOWN_ELEM)
    if tp in SCALARS:
        return SCALARS[tp]
    if tp in TYPEDEFS:
        return TYPEDEFS[tp]
    return UNKNOWN_ELEM


MEMBER = re.compile(r"\s*((?:const\s+)?(?:struct\s+\w+|unsigned\s+\w+|\w+)(?:\s*\*)*)\s+(\*?)(\w+)((?:\s*\[[^\]]+\])*)\s*")


def members_size(body):
    total = 0
    for piece in strip(body).split(";"):
        m = MEMBER.fullmatch(piece)
        if not m:
            continue
        tp, star, _, dims = m.groups()
        sz = 8 if star or tp.endswith("*") else elem_size(tp)
        for d in re.findall(r"\[([^\]]+)\]", dims):
            n = ev(d)
            sz *= n if n is not None else 1
        total += sz
    return total


def load_types(sources):
    """Scalar typedefs and struct sizes (lower bounds) from C text."""
    for text in sources:
        t = strip(text)
        for m in re.finditer(r"typedef\s+((?:unsigned\s+)?\w+)\s+(\w+)\s*;", t):
            TYPEDEFS.setdefault(m.group(2), elem_size(m.group(1)))
        for m in re.finditer(r"typedef\s+struct\s*(\w*)\s*\{([^{}]*)\}\s*(\w+)\s*;", t):
            sz = members_size(m.group(2))
            TYPEDEFS.setdefault(m.group(3), sz)
            if m.group(1):
                STRUCTS.setdefault(m.group(1), sz)
        for m in re.finditer(r"(?<!typedef )struct\s+(\w+)\s*\{([^{}]*)\}\s*;", t):
            STRUCTS.setdefault(m.group(1), members_size(m.group(2)))


for _f in glob.glob("include/*.h") + glob.glob("src/*.c") + glob.glob("ava1/*.h"):
    load_types([open(_f, errors="ignore").read()])

# `type [*]name[dim]...` for locals; the type is a scalar, a typedef, `struct tag` or anything else.
ARR = re.compile(r"(?<![\w.>])((?:const\s+)?(?:struct\s+\w+|unsigned\s+(?:char|short|int|long)|\w+))\s*(\*?)\s*(\w+)((?:\s*\[[^\]]+\])+)\s*(?==|;|,|\))")
KEYWORDS = {"return", "else", "goto", "case", "sizeof", "typedef", "if", "while", "for", "switch", "do"}


def arrays(body):
    """(name, dimension text, bytes or None) for each array declared in `body`."""
    out = []
    for m in ARR.finditer(body):
        tp, star, name, dims = m.groups()
        if tp in KEYWORDS:
            continue
        per = 8 if star else elem_size(tp)
        total, text, known = per, [], True
        for d in re.findall(r"\[([^\]]+)\]", dims):
            text.append(d.strip())
            n = ev(d)
            if n is None:
                known = False
            else:
                total *= n  # a 2-D array is every dimension multiplied
        out.append((name, "][".join(text), total if known else None))
    return out


def calls(name):
    """Functions `name` calls, plus any known function whose name appears in its body (a function
    pointer passed as an argument, e.g. handle_hw_text_op(..., hw_info_get_text, ...))."""
    if name not in FUNCS:
        return set()
    body = strip(FUNCS[name][0])
    out = set(re.findall(r"\b(\w+)\s*\(", body))
    out |= {w for w in re.findall(r"\b\w+\b", body) if w in FUNCS}
    out.discard(name)
    return out


def reach(name):
    seen, todo = set(), [name]
    while todo:
        n = todo.pop()
        if n in seen or n not in FUNCS:
            continue
        seen.add(n)
        todo.extend(calls(n))
    return seen


def table():
    out = []
    for l in open("src/mgmt_table.def"):
        m = re.match(r"MGMT_H([01])\((.*)\)\s*$", l)
        if m:
            f = [x.strip() for x in m.group(2).split(",")]
            out.append(dict(kind=m.group(1), method=f[0], frame=f[1], ack=f[2], flags=f[3], handler=f[4], runner=f[5]))
    return out


def check_table():
    bad, seen = [], set()
    gen = open("ava1/gen/ava1_gen.h").read()
    checklist = open("../protocol/ava1/MGMT_METHODS.md").read()
    for e in table():
        if e["method"] in seen:
            bad.append("duplicate method " + e["method"])
        seen.add(e["method"])
        if "#define " + e["method"] + " " not in gen:
            bad.append("%s is not a generated AVA1_METHOD_* constant" % e["method"])
        if e["handler"] not in FUNCS or FUNCS[e["handler"]][1] != "src/runtime.c":
            bad.append("%s: handler %s is not defined in runtime.c" % (e["method"], e["handler"]))
        dotted = e["method"][len("AVA1_METHOD_"):].lower()
        if "`%s`" % dotted.replace("_", ".", 1) not in checklist and "`%s`" % dotted not in checklist:
            bad.append("%s is not in MGMT_METHODS.md (looked for %s)" % (e["method"], dotted.replace("_", ".", 1)))
    return bad


def check_recv():
    bad = []
    for name, (body, f) in FUNCS.items():
        if f != "src/runtime.c" or not name.startswith("handle_"):
            continue
        if name in TRANSFER_HANDLERS:
            continue
        if "recv_exact(client_fd" in strip(body):
            bad.append("%s reads from client_fd" % name)
    return bad


# The FTX2 transfer-port handlers and the dispatcher read their own bodies; none is a table entry.
TRANSFER_HANDLERS = {"handle_stream_shard", "handle_begin_tx_frame", "handle_binary_frame_impl", "handle_packed_shard"}

SONY_RE = re.compile(r"\b(sceUserService\w*|sceRegMgr\w*|sceAppInstUtil\w*|sceLncUtil\w*|sceSystemService\w*|sony_api_lock\w*)\b")


def sony_api_names():
    api = set()
    for h in ("register", "profile", "sys_registry", "remoteplay", "notif"):
        for l in open("include/%s.h" % h, errors="ignore"):
            if l and l[0] not in " \t/*#}\n" and "(" in l and not l.startswith(("typedef", "struct", "enum")):
                m = re.search(r"(\w+)\s*\(", l)
                if m:
                    api.add(m.group(1))
    return api


def check_sony():
    api, bad = sony_api_names(), []
    for e in table():
        names = set()
        for fn in reach(e["handler"]):
            names |= calls(fn)
            if SONY_RE.search(strip(FUNCS[fn][0])):
                names.add("sony-call")
        hit = sorted(n for n in names if n in api or n == "sony-call")
        if hit and "MGMT_SONY" not in e["flags"]:
            bad.append("%s (%s) reaches %s but lacks MGMT_SONY" % (e["method"], e["handler"], ", ".join(hit[:4])))
    return bad


def findings(handler, floor):
    out = []
    for fn in sorted(reach(handler)):
        for (n, expr, sz) in arrays(strip(FUNCS[fn][0])):
            if sz is not None and sz >= floor:
                out.append((fn, FUNCS[fn][1].split("/")[-1], n, expr, sz))
    return out


def check_stack():
    bad = []
    for e in table():
        for (fn, f, n, expr, sz) in findings(e["handler"], LIMIT):
            if (e["handler"], fn, n) not in ALLOW:
                bad.append("%s: %s (%s) has %s[%s] = %d bytes on the stack" % (e["method"], fn, f, n, expr, sz))
    return bad


def selftest():
    bad = []
    load_types(["typedef unsigned char byte_t;\ntypedef struct { char name[300]; uint64_t id; } rec_t;\nstruct big { rec_t r[100]; int n; };"])
    cases = [
        ("struct array", "void f(void) { struct big b[2]; }", 2 * (100 * (300 + 8) + 4)),
        ("typedef struct array", "void f(void) { rec_t r[64]; }", 64 * 308),
        ("typedef scalar array", "void f(void) { byte_t buf[20000]; }", 20000),
        ("2-D array", "void f(void) { char tab[64][512]; }", 64 * 512),
        ("3-D array", "void f(void) { uint32_t t[4][8][16]; }", 4 * 8 * 16 * 4),
        ("pointer array", "void f(void) { char *names[4096]; }", 4096 * 8),
        ("unknown element type is 8 bytes", "void f(void) { mystery_t m[3000]; }", 3000 * 8),
        ("macro dimension", "#define N (4 * 1024)\nvoid f(void) { uint64_t q[N]; }", 4 * 1024 * 8),
    ]
    DEFS["N"] = "(4 * 1024)"
    for label, code, want in cases:
        got = [sz for (_, _, sz) in arrays(strip(code))]
        if want not in got:
            bad.append("%s: expected %d bytes, saw %s" % (label, want, got))
    # function-pointer argument: the callee named only as an argument is followed
    FUNCS["selftest_outer"] = ("int selftest_outer(void) { return selftest_helper(0, selftest_getter); }\n}\n", "src/x.c")
    FUNCS["selftest_helper"] = ("int selftest_helper(int a, int (*g)(void)) { return g(); }\n}\n", "src/x.c")
    FUNCS["selftest_getter"] = ("int selftest_getter(void) { char big[20000]; return big[0]; }\n}\n", "src/x.c")
    hit = [f[0] for f in findings("selftest_outer", LIMIT)]
    if "selftest_getter" not in hit:
        bad.append("a function passed as an argument was not followed")
    return bad


def report():
    md = open("../protocol/ava1/MGMT_METHODS.md").read()
    skip = {"handle_begin_tx_frame", "handle_query_tx_frame", "handle_commit_tx_frame", "handle_abort_tx_frame", "handle_stream_shard", "handle_status_frame"}
    for h in re.findall(r"`src/runtime.c:\d+` `(\w+)`", md):
        if h in skip:
            continue
        for (fn, f, n, expr, sz) in findings(h, REPORT_MIN):
            print("%-34s %-34s %-12s %-10s %-26s %6d%s" % (h, fn, f, n, expr, sz, "  (>=16K)" if sz >= LIMIT else ""))
    return []


if __name__ == "__main__":
    cmd = sys.argv[1] if len(sys.argv) > 1 else "all"
    checks = dict(table=check_table, recv=check_recv, sony=check_sony, stack=check_stack, report=report, selftest=selftest)
    todo = ["table", "recv", "sony", "stack"] if cmd == "all" else [cmd]
    failed = 0
    for c in todo:
        for line in checks[c]():
            print("%s: %s" % (c, line))
            failed = 1
    sys.exit(failed)
