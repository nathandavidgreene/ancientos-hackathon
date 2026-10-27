#!/usr/bin/env python3
"""Generate the reference pages for the MkDocs site.

Reads `//!` module docs and `///` item docs straight from `crates/*/src`
and writes `docs/reference/<crate>.md` (generated, git-ignored). Each module
section links to its rustdoc page under `docs/api/` (built by
`tools/build_docs.sh`).
"""

from __future__ import annotations

import ast
import re
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "docs" / "reference"
ITEM_RE = re.compile(
    r"^pub(?:\([^)]*\))?\s+(?:const\s+fn|async\s+fn|fn|struct|enum|trait|const|static|type)\s+"
    r"([A-Za-z_][A-Za-z0-9_]*)"
)
KIND_RE = re.compile(r"^pub(?:\([^)]*\))?\s+(const\s+fn|async\s+fn|fn|struct|enum|trait|const|static|type)")


RUSTDOC_LINK_RE = re.compile(r"\[([^\]]+)\]\((?:crate|super|self|[A-Za-z_][A-Za-z0-9_]*)::[^)]*\)")
BARE_LINK_RE = re.compile(r"\[(`[^`]+`)\](?!\()")


def plain_links(text: str) -> str:
    """Turn rustdoc intra-doc links into plain code spans for MkDocs."""
    return BARE_LINK_RE.sub(r"\1", RUSTDOC_LINK_RE.sub(r"\1", text))


def module_doc(lines: list[str]) -> str:
    """Return the leading `//!` block of a source file as markdown."""
    out: list[str] = []
    for line in lines:
        s = line.strip()
        if s.startswith("//!"):
            out.append(s[3:][1:] if s[3:].startswith(" ") else s[3:])
        elif out or s == "":
            if out:
                break
        else:
            break
    return plain_links("\n".join(out).strip())


def items(lines: list[str]) -> list[tuple[str, str, str]]:
    """Return top-level pub items as (kind, name, first doc line), skipping tests."""
    found: list[tuple[str, str, str]] = []
    for i, line in enumerate(lines):
        if line.startswith("#[cfg(test)]"):
            break
        m = ITEM_RE.match(line)
        if not m or line.startswith("pub(crate)"):
            continue
        kind = KIND_RE.match(line).group(1).replace("const fn", "fn").replace("async fn", "fn")  # type: ignore[union-attr]
        j = i - 1
        doc: list[str] = []
        while j >= 0 and (lines[j].strip().startswith("///") or lines[j].strip().startswith("#[")):
            s = lines[j].strip()
            if s.startswith("///"):
                doc.insert(0, s[3:].strip())
            j -= 1
        first = ""
        for d in doc:
            if d:
                first = d
                break
        found.append((kind, m.group(1), first))
    return found


API_DIR = ROOT / "docs" / "api"


def exists_in_api(href: str) -> bool:
    """True if a `../api/...` href points at a page rustdoc actually generated."""
    return (API_DIR / href.removeprefix("../api/")).exists()


def rustdoc_href(crate_ident: str, module: str | None, kind: str, name: str) -> str:
    """Relative link from docs/reference/<crate>.html to the rustdoc page of an item."""
    base = f"../api/{crate_ident}/"
    if module:
        base += f"{module}/"
    prefix = {"fn": "fn", "struct": "struct", "enum": "enum", "trait": "trait",
              "const": "constant", "static": "static", "type": "type"}[kind]
    return f"{base}{prefix}.{name}.html"


def crate_page(crate_dir: Path) -> tuple[str, str]:
    """Build (package name, markdown) for one crate."""
    pkg = tomllib.loads((crate_dir / "Cargo.toml").read_text())["package"]["name"]
    ident = pkg.replace("-", "_")
    src = crate_dir / "src"
    has_lib = (src / "lib.rs").exists()
    md = [f"# {pkg}", "", "_Generated from the source doc comments by `tools/gen_module_pages.py`._", ""]
    if has_lib:
        md += [f"[Full API reference (rustdoc)](../api/{ident}/index.html){{ target=_blank }}", ""]
    files = sorted(src.rglob("*.rs"), key=lambda p: (p.name not in ("lib.rs", "main.rs"), str(p)))
    for f in files:
        lines = f.read_text(encoding="utf-8").splitlines()
        rel = f.relative_to(src)
        if rel.name == "lib.rs":
            module, title = None, "crate root (`lib.rs`)"
        elif rel.name == "main.rs" or rel.parts[0] == "bin":
            module, title = None, f"binary (`{rel}`)"
        else:
            module = rel.with_suffix("").as_posix().replace("/", "::")
            title = f"`{module}`"
        md += [f"## {title}", ""]
        doc = module_doc(lines)
        if doc:
            md += [doc, ""]
        mod_href = f"../api/{ident}/{module.replace('::', '/')}/index.html" if module else ""
        if module and has_lib and exists_in_api(mod_href):
            md += [f"[rustdoc]({mod_href}){{ target=_blank }}", ""]
        its = items(lines) if (has_lib and rel.name != "main.rs" and rel.parts[0] != "bin") else []
        if its:
            md += ["| Item | Kind | Summary |", "|---|---|---|"]
            for kind, name, first in its:
                href = rustdoc_href(ident, module, kind, name)
                summary = plain_links(first).replace("|", "\\|")
                cell = f"[`{name}`]({href})" if exists_in_api(href) else f"`{name}`"
                md.append(f"| {cell} | {kind} | {summary} |")
            md.append("")
    return pkg, "\n".join(md)


def cli_page() -> str:
    """Build the CLI reference from every binary's `//!` doc."""
    md = ["# CLI reference", "", "_Generated from each binary's `//!` doc by `tools/gen_module_pages.py`._", ""]
    for crate_dir in sorted((ROOT / "crates").iterdir()):
        src = crate_dir / "src"
        bins = sorted([src / "main.rs", *sorted((src / "bin").glob("*.rs"))])
        for f in bins:
            if not f.exists():
                continue
            doc = module_doc(f.read_text(encoding="utf-8").splitlines())
            md += [f"## `{crate_dir.name}`: `{f.relative_to(crate_dir)}`", "", doc or "_No module doc._", ""]
    return "\n".join(md)


def python_page() -> str:
    """Build the Python tools reference from module and function docstrings."""
    md = ["# Python build tools", "", "_Generated from the docstrings in `tools/*.py`._", ""]
    for f in sorted((ROOT / "tools").glob("*.py")):
        tree = ast.parse(f.read_text(encoding="utf-8"))
        md += [f"## `tools/{f.name}`", "", ast.get_docstring(tree) or "", ""]
        for node in tree.body:
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                args = ", ".join(a.arg for a in node.args.args)
                md += [f"### `{node.name}({args})`", "", ast.get_docstring(node) or "", ""]
    return "\n".join(md)


def main() -> None:
    """Write one reference page per workspace crate, plus the CLI and Python pages."""
    OUT.mkdir(parents=True, exist_ok=True)
    for crate_dir in sorted((ROOT / "crates").iterdir()):
        if (crate_dir / "Cargo.toml").exists():
            pkg, md = crate_page(crate_dir)
            (OUT / f"{pkg}.md").write_text(md + "\n", encoding="utf-8")
            print(f"wrote docs/reference/{pkg}.md")
    (OUT / "cli.md").write_text(cli_page() + "\n", encoding="utf-8")
    (OUT / "python-tools.md").write_text(python_page() + "\n", encoding="utf-8")
    print("wrote docs/reference/cli.md, docs/reference/python-tools.md")


if __name__ == "__main__":
    main()
