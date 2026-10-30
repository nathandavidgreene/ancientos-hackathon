# API reference

## Rust (rustdoc)

Generated with `cargo doc --no-deps --workspace --document-private-items` and
copied into the site by `tools/build_docs.sh`, so private functions and their
doc comments are included. [Open in a new tab](api/index.html){ target=_blank }.

<iframe src="api/index.html" style="width:100%; height:80vh; border:1px solid #ccc;"></iframe>

## Python

The only Python in the repo is the docs tooling under `tools/`. Its reference
is generated from the docstrings: [Python build tools](reference/python-tools.md).

Decision-log signing: every application opens logs using `ANCIENTOS_LOG_KEY_PATH`.
Unset preserves unsigned operation with a loud warning. A configured key must
be at least 32 bytes and mode 0600. Existing unsigned logs cannot be opened
in signed mode: retain the old log unchanged and configure a new log path
when enabling signing. Restart the affected binaries after configuring it.

Local model URLs accept only loopback, exact `localhost`, or RFC1918 IP
literals by default. Redirects and proxy routing are disabled for local prompt
transport. `ANCIENTOS_ALLOW_REMOTE_LOCAL=1` explicitly permits a remote server;
use it only when that destination is intended to receive private data.
