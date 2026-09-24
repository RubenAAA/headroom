#!/usr/bin/env python3
"""Split a large Rust module into child modules by moving items only.

    split-rust-module.py split  <file.rs> <plan.py>
    split-rust-module.py verify <file.rs> [<git-rev>]   (default HEAD)

`split` moves the top-level items the plan names from <file.rs> into
<dir>/<module>.rs next to it (proxy.rs -> proxy/<module>.rs). Each item keeps
its bytes, plus the blank lines and comments between it and the item before.
The mechanical edits, and nothing else:

- private items, struct fields and inherent methods become `pub(super)`, so
  the parent and sibling modules still reach them;
- items already `pub(super)` become `pub(in <parent>)` (or `pub(crate)`),
  so the module above keeps reaching them;
- `super::` in moved code is spelled as the parent's absolute path, since
  one level deeper it would name a different module;
- each child starts with `use super::*;`, and the file glob re-exports every
  child, so no caller path changes and `pub` API stays `pub`;
- inline test modules go to the end of their new file. Modules listed in
  TEST_FILES instead become file modules with their contents untouched.

The plan is a Python file defining
    PLAN = {"module": ("doc line(s)", ["item", ...]), ...}
    TEST_FILES = ["tests", ...]          # optional
Item names are what scripts/itemspan prints: fn/struct/const idents, impls
as `Type` or `Trait@for@Type`. A bare name takes every item with that name;
`name#k` takes the k-th. Unlisted items stay put.

`verify` checks the result against <git-rev>: every item of the old file must
appear in the new file or its children with the same text, ignoring
whitespace, trailing commas rustfmt adds, visibility and the `super::`
spelling, and each TEST_FILES body must match its old inline module.

Run rustfmt on <file.rs> after `split` (it follows the new `mod` lines), then
build, then `verify`. Work in a separate worktree when other sessions share
the checkout.
"""
import collections
import re
import runpy
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
ITEMSPAN_DIR = HERE / "itemspan"


def itemspan_bin():
    target = ITEMSPAN_DIR / "target"
    exe = target / "release" / "itemspan"
    if not exe.exists():
        subprocess.run(
            ["cargo", "build", "--release", "-q", "--manifest-path", str(ITEMSPAN_DIR / "Cargo.toml"),
             "--target-dir", str(target)],
            check=True,
        )
    return str(exe)


def items_of(path):
    """[(start, end, kind, name)] for each top-level item, 1-based inclusive."""
    out = subprocess.run([itemspan_bin(), str(path)], capture_output=True, text=True, check=True).stdout
    rows = []
    for row in out.splitlines():
        start, end, kind, name = row.split(" ", 3)
        rows.append((int(start), int(end), kind, name))
    return rows


def module_paths(file):
    """(`crate::a::b` for the file, its parent's path, dir for children)."""
    parts = file.resolve().parts
    src = len(parts) - 1 - parts[::-1].index("src")
    rel = list(parts[src + 1 :])
    if rel[-1] in ("lib.rs", "main.rs"):
        sys.exit("split a submodule, not the crate root")
    if rel[-1] == "mod.rs":
        rel = rel[:-1]
        child_dir = file.parent
    else:
        rel[-1] = rel[-1][:-3]
        child_dir = file.parent / rel[-1]
    path = "::".join(["crate"] + rel)
    parent = "::".join(["crate"] + rel[:-1])
    return path, parent, child_dir


PRIVATE_DECL = re.compile(r"^(?P<indent>\s*)(?:async\s+|unsafe\s+|const\s+(?=fn))*(?:fn|struct|enum|const|static|type|trait)\b")
FIELD = re.compile(r"^\s*(?:r#)?[a-z_][a-z0-9_]*\s*:")


def adjust(lines, kind, parent):
    """Visibility and `super::` edits for one moved item (see module doc)."""
    widened_super = "pub(crate)" if parent == "crate" else f"pub(in {parent})"
    out = []
    depth = 0
    for line in lines:
        line = re.sub(r"(?<![\w:])super::(?!\*)", f"{parent}::", line)
        stripped = line.lstrip()
        indent = line[: len(line) - len(stripped)]
        at_item = depth == 0 and not line.startswith((" ", "\t"))
        in_body = depth == 1 and kind in ("impl", "struct")
        if (at_item or in_body) and stripped.startswith("pub(super)"):
            line = indent + widened_super + stripped[len("pub(super)"):]
        elif (at_item or in_body) and stripped and not stripped.startswith(("pub", "//", "#", "}")):
            decl = PRIVATE_DECL.match(line)
            if decl and at_item:
                line = f"{indent}pub(super) {stripped}"
            elif decl and in_body and kind == "impl" and re.match(r"\s*(?:async\s+|unsafe\s+|const\s+)*fn\b|\s*const\b", line):
                line = f"{indent}pub(super) {stripped}"
            elif in_body and kind == "struct" and FIELD.match(line):
                line = f"{indent}pub(super) {stripped}"
        out.append(line)
        code = re.sub(r'"(?:\\.|[^"\\])*"', '""', line.split("//")[0])
        code = re.sub(r"'(?:\\.|[^'\\])'", "''", code)
        depth += code.count("{") - code.count("}")
    return out


def split(file, plan_path):
    file = Path(file)
    plan = runpy.run_path(plan_path)
    PLAN, TEST_FILES = plan["PLAN"], plan.get("TEST_FILES", [])
    _, parent, child_dir = module_paths(file)
    src = file.read_text().splitlines(keepends=True)

    # A new `mod x` shadows any `x` the moved code reached through a glob
    # (`compression::Foo` meaning `crate::compression`). That fails to build
    # at best, and resolves to the wrong item at worst.
    code = "".join(l.split("//")[0] for l in src)
    roots = set(re.findall(r"(?<![\w:])([a-z_][a-z0-9_]*)::", code))
    clash = sorted(set(PLAN) & roots)
    if clash:
        sys.exit(f"module names already used as path roots in {file.name}: {clash}; rename them")

    wanted = {}
    for module, (_, names) in PLAN.items():
        for n in names:
            if n in wanted:
                sys.exit(f"{n} assigned twice")
            wanted[n] = module
    for n in TEST_FILES:
        wanted[n] = None

    # Segment = previous item end + 1 .. this item end: carries the blank
    # lines and plain comments that sit above an item.
    segments, prev_end, seen = [], 0, collections.Counter()
    for _, end, kind, name in items_of(file):
        seen[name] += 1
        key = f"{name}#{seen[name]}"
        segments.append((prev_end + 1, end, kind, key if key in wanted else name))
        prev_end = end
    missing = sorted(set(wanted) - {s[3] for s in segments})
    if missing:
        sys.exit(f"not found in {file.name}: {missing}")

    child_dir.mkdir(exist_ok=True)
    taken, replaced = set(), {}
    bodies = {m: [] for m in PLAN}
    tails = {m: [] for m in PLAN}
    for s, e, kind, name in segments:
        if name not in wanted:
            continue
        chunk = src[s - 1 : e]
        taken.update(range(s, e + 1))
        if wanted[name] is None:
            head = next(i for i, l in enumerate(chunk) if re.match(rf"^mod {name} \{{$", l))
            assert chunk[-1].rstrip() == "}", name
            (child_dir / f"{name}.rs").write_text("".join(chunk[head + 1 : -1]).lstrip("\n"))
            replaced[s] = "".join(chunk[:head]) + f"mod {name};\n"
            continue
        if kind == "mod":
            # Test modules go last in their file (clippy: items_after_test_module).
            tails[wanted[name]].append("".join(chunk))
            continue
        if not (kind == "impl" and "@for@" in name):
            chunk = [l + "\n" for l in adjust([l.rstrip("\n") for l in chunk], kind, parent)]
        else:
            chunk = [re.sub(r"(?<![\w:])super::(?!\*)", f"{parent}::", l) for l in chunk]
        bodies[wanted[name]].append("".join(chunk))

    kept = "".join(replaced.get(i, "") if i in taken else l for i, l in enumerate(src, 1))
    modules = list(PLAN)
    wiring = "".join(f"mod {m};\n" for m in modules) + (
        "\n// Globs cap each item at its own visibility: `pub` items stay public\n"
        "// API, the rest stay in-crate. Modules with no `pub` item would\n"
        "// otherwise warn that they re-export nothing.\n"
        "#[allow(unused_imports)]\npub use self::{\n"
        + "".join(f"    {m}::*,\n" for m in modules)
        + "};\n\n"
    )
    lines = kept.splitlines(keepends=True)
    mod_lines = [i for i, l in enumerate(lines) if re.match(r"^(pub(\([^)]*\))? )?mod \w+;$", l)]
    if mod_lines:
        # Join the first run of `mod` lines so rustfmt sorts them as one
        # group. Later ones are test-file stubs written above.
        at = mod_lines[0] + 1
        while at in mod_lines:
            at += 1
    else:
        at = next(i for i, l in enumerate(lines) if not l.startswith("//!") and l.strip())
    lines[at:at] = [wiring]
    file.write_text(re.sub(r"\n{3,}", "\n\n", "".join(lines)))

    for module, (doc, _) in PLAN.items():
        header = (
            "".join(f"//! {l}".rstrip() + "\n" for l in doc.splitlines())
            + f"//!\n//! Moved out of `{file.name}` without behavior change. `use super::*`\n"
            "//! keeps the parent's items and imports in reach; the parent re-exports\n"
            "//! this module, so callers keep their paths.\n\nuse super::*;\n\n"
        )
        body = "".join(bodies[module] + tails[module]).lstrip("\n")
        (child_dir / f"{module}.rs").write_text(header + body)
    print(f"{file.name}: {len(src)} -> {len(file.read_text().splitlines())} lines")
    for m in modules:
        print(f"  {m}.rs: {len(bodies[m]) + len(tails[m])} items")


def normalize(text, parent):
    text = re.sub(r"\bpub\((?:super|crate|self|in [\w:]+)\)\s*", "", text)
    text = text.replace(f"{parent}::", "@::")
    text = re.sub(r"(?<![\w:])super::", "@::", text)
    text = re.sub(r"\s+", "", text)
    return re.sub(r",([)\]}])", r"\1", text)


def verify(file, rev="HEAD"):
    file = Path(file)
    _, parent, child_dir = module_paths(file)
    root = subprocess.run(["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True,
                          cwd=file.parent, check=True).stdout.strip()
    rel = file.resolve().relative_to(root)
    old_src = subprocess.run(["git", "show", f"{rev}:{rel}"], capture_output=True, text=True,
                             cwd=root, check=True).stdout
    old_path = Path("/tmp") / f"split-verify-{file.name}"
    old_path.write_text(old_src)

    def texts(path, src):
        lines = src.splitlines()
        return [(k, n, "\n".join(lines[s - 1 : e])) for s, e, k, n in items_of(path) if k != "use"]

    files = [file] + sorted(child_dir.glob("*.rs"))
    test_files = {f.stem for f in files[1:]
                  if re.search(rf"^mod {f.stem} \{{$", old_src, re.M)}
    old = collections.Counter((k, n, normalize(t, parent)) for k, n, t in texts(old_path, old_src)
                              if not (k == "mod" and n in test_files))
    new = collections.Counter()
    for f in files:
        for k, n, t in texts(f, f.read_text()):
            new[(k, n, normalize(t, parent))] += 1
    unmatched = old - new
    ok = not unmatched
    print(f"{sum(old.values())} items in {rev}:{rel}; {sum(unmatched.values())} unmatched")
    for k, n, _ in unmatched:
        print(f"  UNMATCHED {k} {n}")
    for name in sorted(test_files):
        found = re.search(rf"^mod {name} \{{\n(.*?)\n\}}\n", old_src, re.M | re.S)
        assert found, name
        body = found.group(1)
        same = normalize(body, parent) == normalize((child_dir / f"{name}.rs").read_text(), parent)
        ok &= same
        print(f"  test file {name}.rs matches old inline module: {same}")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    if len(sys.argv) >= 4 and sys.argv[1] == "split":
        split(sys.argv[2], sys.argv[3])
    elif len(sys.argv) in (3, 4) and sys.argv[1] == "verify":
        verify(*sys.argv[2:])
    else:
        sys.exit(__doc__)
