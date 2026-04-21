"""Webhook HTTP server — stdlib only.

Mirrors ``WebhookServer.swift``. A single ``POST /webhook`` endpoint
(plus bare ``/`` for legacy hook URLs that pre-date the path move)
validates the GitHub HMAC-SHA256 signature and dispatches decoded
events to a callback.

Using ``ThreadingHTTPServer`` from the standard library rather than
aiohttp or FastAPI keeps the daemon's footprint small — we're serving
one endpoint at low request rate, and avoiding an extra dependency is
worth the small amount of boilerplate.
"""

from __future__ import annotations

import logging
import threading
from collections.abc import Callable
from dataclasses import dataclass
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

logger = logging.getLogger(__name__)


DeliveryHandler = Callable[[dict[str, str], bytes], "HandlerResult"]


@dataclass(frozen=True)
class HandlerResult:
    status: int
    body: str

    @staticmethod
    def ok() -> HandlerResult:
        return HandlerResult(status=HTTPStatus.OK, body="ok\n")

    @staticmethod
    def unauthorized() -> HandlerResult:
        return HandlerResult(status=HTTPStatus.UNAUTHORIZED, body="bad signature\n")

    @staticmethod
    def bad_request() -> HandlerResult:
        return HandlerResult(status=HTTPStatus.BAD_REQUEST, body="bad request\n")


class WebhookServer:
    """Binds a ThreadingHTTPServer on localhost + handles ``POST /webhook``.

    Lifecycle:
        srv = WebhookServer(delivery_handler)
        port = srv.start()          # returns bound port
        ...
        srv.stop()

    The listener runs on a background thread; ``start()`` blocks only
    until the socket is bound.
    """

    def __init__(self, delivery_handler: DeliveryHandler) -> None:
        self._delivery_handler = delivery_handler
        self._server: ThreadingHTTPServer | None = None
        self._thread: threading.Thread | None = None

    def start(self) -> int:
        handler_cls = _make_handler(self._delivery_handler)
        # Bind to 127.0.0.1 and let the OS pick a port.
        server = ThreadingHTTPServer(("127.0.0.1", 0), handler_cls)
        self._server = server
        self._thread = threading.Thread(
            target=server.serve_forever,
            name="shipyard-webhook-server",
            daemon=True,
        )
        self._thread.start()
        return server.server_address[1]

    def stop(self) -> None:
        if self._server is not None:
            self._server.shutdown()
            self._server.server_close()
            self._server = None
        if self._thread is not None:
            self._thread.join(timeout=2)
            self._thread = None


def _make_handler(delivery_handler: DeliveryHandler) -> type[BaseHTTPRequestHandler]:
    class _Handler(BaseHTTPRequestHandler):
        # Silence the default request-logger; we emit through our logger.
        def log_message(self, format: str, *args: Any) -> None:  # noqa: A002
            logger.debug("%s - - [%s] %s", self.address_string(), self.log_date_time_string(), format % args)

        def do_POST(self) -> None:  # noqa: N802 — BaseHTTPRequestHandler API
            path = self.path.split("?", 1)[0]
            # Accept /webhook (what we register going forward) AND
            # the bare / that pre-path-move hooks POST to.
            if path not in ("/webhook", "/"):
                self._reply(HandlerResult(status=HTTPStatus.NOT_FOUND, body="not found\n"))
                return
            try:
                length = int(self.headers.get("content-length", "0"))
            except ValueError:
                self._reply(HandlerResult.bad_request())
                return
            if length < 0 or length > 5 * 1024 * 1024:
                self._reply(HandlerResult.bad_request())
                return
            body = self.rfile.read(length) if length > 0 else b""
            headers = {k.lower(): v for k, v in self.headers.items()}
            result = delivery_handler(headers, body)
            self._reply(result)

        def do_GET(self) -> None:  # noqa: N802
            # Any GET gets 405 — the hook endpoint is POST-only. This
            # also stops curious browsers from lighting up 404 logs.
            self._reply(
                HandlerResult(
                    status=HTTPStatus.METHOD_NOT_ALLOWED,
                    body="method not allowed\n",
                )
            )

        def _reply(self, result: HandlerResult) -> None:
            body_bytes = result.body.encode("utf-8")
            self.send_response(result.status)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.send_header("Content-Length", str(len(body_bytes)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(body_bytes)

    return _Handler
