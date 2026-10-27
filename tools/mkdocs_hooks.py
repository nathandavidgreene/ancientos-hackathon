"""MkDocs build hooks for the AncientOS docs site."""

from __future__ import annotations

import re
from typing import Any

SOURCE_LINK_RE = re.compile(r"\[([^\]]+)\]\(\.\./(crates/[^)]*)\)")


def on_page_markdown(markdown: str, **_kwargs: Any) -> str:
    """Turn repo-relative source links (`../crates/...`) into plain text plus path.

    The repo docs link to source files for GitHub readers; the site has no
    source tree, so the link becomes `text` followed by the source path.
    """
    return SOURCE_LINK_RE.sub(lambda m: f"{m.group(1)} (`{m.group(2)}`)", markdown)
