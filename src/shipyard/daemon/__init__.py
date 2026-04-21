"""Webhook receiver daemon.

Ports the live-mode pipeline originally implemented in the macOS menu-
bar app (``shipyard-macos-gui``) so it can run on any platform and be
consumed by any CLI command. See Shipyard issue #125 for the design
and tracking. The GUI becomes a thin subscriber to this daemon.

The daemon is composed of several small modules that mirror the Swift
implementation; the goal is byte-for-byte behavioral parity on the
things that matter (HMAC validation, event decoding, Tailscale
readiness rules, idempotent tunnel reconcile) so the macOS app can
swap its in-process webhook server for a subprocess without users
noticing.
"""
