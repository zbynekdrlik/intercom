# Vendored third-party assets — `intercom/web/`

## `janus.js`

- **Source:** the official Janus WebRTC Server JavaScript library from the meetecho/janus-gateway
  repository, file `html/janus.js`.
- **Tag / version:** `v1.1.2` — pinned to match the `janus` **1.1.2** package Ubuntu 26.04 ships
  on strih-lx (the WebRTC edge, per the issue-1345 M3 design). Keep the vendored `janus.js` tag in
  lock-step with the deployed Janus server version when the OS package moves.
- **Upstream URL:**
  `https://raw.githubusercontent.com/meetecho/janus-gateway/v1.1.2/html/janus.js`
- **Licence:** MIT (© 2016 Meetecho). The full MIT licence header is preserved verbatim at the top
  of the vendored file — do NOT strip it.
- **Local modifications:** none. The file is committed byte-for-byte as fetched, so the licence
  header and behaviour are exactly upstream's. Re-fetch from the matching tag to update; never
  hand-edit.
- **Why vendored (not a CDN):** the phone PWA is served embedded from the hub binary
  (`include_str!`) so it is self-contained on strih-lx and works on the venue LAN with no internet
  and no third-party CDN dependency (the same self-contained model as the bkshading panel).
