"""Tunnel backends for the webhook daemon.

Tunnel backends expose a local TCP port to the public internet so
GitHub can POST webhooks to it. The daemon ships Tailscale Funnel
in v1; Cloudflare Tunnel / ngrok / user-supplied reverse-proxy
backends are tracked in Shipyard #126.

All backends implement ``TunnelBackend`` in ``base.py``.
"""
