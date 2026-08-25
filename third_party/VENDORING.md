# Vendored dependencies

The two directories in this folder are kept as source subtrees so the plugin
build is reproducible without a localhost helper process:

- `irlserver-srt` — BELABOX/IRLServer SRT fork, MPL-2.0 upstream license.
- `srtla_send` — IRLServer SRTLA sender, MIT upstream license; the embedded
  endpoint reuses its `srtla-core` and `srtla-protocol` crates.

When updating either subtree, record the upstream commit and review the local
changes in the transport/embedded seams. Do not collapse these files into the
GPL plugin license; the upstream notices remain authoritative.
