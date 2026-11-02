---
name: verify
description: Verify herdr-sim sidebar changes against an isolated Herdr server and inspect the resulting pane tokens.
---

1. Build with `cargo build`. If macOS linking cannot find `iconv`, use:
   ```sh
   SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk \
   CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER=/usr/bin/clang \
   LIBRARY_PATH=/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/usr/lib cargo build
   ```
2. Create a temporary directory. Remove inherited `HERDR_*` variables from the
   test environment, set `XDG_CONFIG_HOME` to that directory,
   `HERDR_CONFIG_PATH` to `<temp>/herdr/config.toml`, and `HERDR_SOCKET_PATH`
   to `<temp>/herdr/herdr.sock`. Create the config with onboarding and version
   checks disabled. Run `herdr server` in a persistent exec session.
3. With that same environment, create a workspace outside Git. Read its pane ID
   from `result.root_pane.pane_id`. Use `pane report-agent` to mark the test
   shell as an idle Codex fixture; no actual coding agent needs to run.
4. Seed stale tokens with `pane report-metadata <pane> --source herdr-sim`.
   Run `target/debug/herdr-sim` with `HERDR_PLUGIN_STATE_DIR=<temp>/plugin`.
   Inspect `herdr pane get <pane>`: sessionless panes must have no `title`,
   and their heading and state labels must not contain the stale title.
   Repeat using a fresh state directory and `HERDR_PLUGIN_EVENT=pane.focused`
   to exercise the status-only path. Run each twice to check stable results.
   For held-state changes, use one state directory across separate plugin
   invocations: report `blocked` then `idle` before any work and require
   `heading_idle`; report `working`, `blocked`, then `idle` and require
   `heading_blocked`. A legacy cache with a held blocked badge but no `worked`
   field must keep that badge. Repeat each final state to check persistence.
5. Capture before/after tokens, then stop only the isolated server with its
   environment. The installed plugin and user server remain untouched.
