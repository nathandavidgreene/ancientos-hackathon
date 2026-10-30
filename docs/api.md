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

Phone and labels authentication: set `ANCIENTOS_LABELS_TOKEN` for a separate
labels/deletion scope. If it is unset, labels falls back to
`ANCIENTOS_PHONE_TOKEN` so existing phone deployments keep working. An empty
or weak configured labels token disables labels authentication instead of
falling back. Generate a random token with `web-ui --print-phone-token`.
Use an HTTPS-terminating proxy: the Secure cookies require HTTPS. Five failed
logins per client in a minute temporarily block that client. For a loopback direct
peer, the limiter uses the trimmed, rightmost `X-Forwarded-For` entry parsed as an
IP address. Tailscale Serve appends the client IP; nginx must pass that header
through unchanged. Missing, empty or malformed rightmost entries fall back to the
shared peer bucket. Duplicate headers are rejected with 400. Non-loopback peers
always use their own IP and cannot override it with this header.

Decision-log privacy: `/log` requires the phone credential, initially via
`/log?k=TOKEN` over HTTPS. New Wonder questions and answers live in private
content-addressed files beside the log; payloads contain digests and short
summaries. Preserve these directories with the log. Runner prompt payloads
contain a basename: place prompt files in the runner repository root before
proposal/execution. Re-propose older pending requests that stored absolute
paths or lack private content files. Existing log entries remain untouched.


Desktop write protection: every non-GET request requires the existing
`ANCIENTOS_PHONE_TOKEN` credential (`ap_k` cookie or form `k`), a session-bound
hidden `csrf` form field, and matching allowed `Origin` and `Host` headers.
`/log` also requires this credential. Missing configuration or request evidence
returns 403 before routing. Labels retain their additional labels credential check;
with separate credentials, log into the desktop first, then labels.

Log into the desktop using `/?k=TOKEN` over HTTPS; it redirects to `/` and sets
an HttpOnly, Secure, SameSite=Strict `ap_k` cookie with `Path=/`. Rendered forms
include a CSRF token bound to a random `ap_session` cookie and this server process.
Reload forms after a server restart. Phone and labels forms use the same CSRF
protection. Programmatic writes must first GET a page, retain its cookies and
hidden token, then POST with matching Origin/Host headers and the credential.

`ANCIENTOS_UI_ALLOWED_ORIGINS` is a comma-separated list of exact HTTP(S)
origins, without trailing slashes, paths, or credentials. By default only the
configured loopback bind host and port are allowed. For the tailnet HTTPS proxy,
configure:

```sh
export ANCIENTOS_UI_ALLOWED_ORIGINS=https://ser6buzz.tail3ab3aa.ts.net:8445
```

The proxy must preserve `Host: ser6buzz.tail3ab3aa.ts.net:8445` and the browser's
`Origin: https://ser6buzz.tail3ab3aa.ts.net:8445`. Forwarded headers are not trusted.
Add the loopback origin explicitly, separated by a comma, if both access paths
are needed. An invalid or empty configured allowlist prevents startup. No proxy
hostname is implicitly trusted.
