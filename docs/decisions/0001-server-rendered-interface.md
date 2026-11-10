# 1. The interface is rendered on the server

**Status:** accepted for 0.2–0.3; to be revisited when dashboards need
drag-and-drop.

## Context

Open Beam's goals are speed, security and a codebase every contributor can
read. A browser interface can be built two ways:

- **Server-rendered pages.** The Rust server sends finished HTML. Every
  interaction is a link or a form.
- **A client application** (for example React and TypeScript). The browser
  runs a program that calls an API.

## Decision

Pages are rendered on the server with `maud` and contain no JavaScript.

## Why

- **Security.** With no scripts, the content security policy forbids all
  scripts. A whole class of attack (cross-site scripting) has nothing to run.
- **Speed.** Pages arrive ready to display; there is no bundle to download and
  start. Charts are SVG drawn on the server.
- **One language, one build.** Contributors need Rust only. There is no second
  toolchain or package tree to keep patched.
- **Every view is an address.** Filters, sorting and columns live in the URL,
  so a view can be bookmarked, shared, downloaded or turned into a feed address.

## Costs

- Every change of filter or sort reloads the page. On a local machine this is
  fast, but it is not as fluid as a client application.
- Drag-and-drop dashboards, live-editing grids and instant previews are hard
  without scripts.

## When to revisit

When a feature genuinely needs in-page interaction (the first is expected to
be arranging dashboard panels), add a small TypeScript module for that page
only. It will be built into a single file served from Open Beam itself, so the
content policy can allow `script-src 'self'` and nothing else: no third-party
scripts, no inline code, no change to how the rest of the interface works.
