"""FastAPI application factory.

Serves JSON and the static page, and nothing else. There is no templating: the front
end is one HTML file and a handful of ES modules, loaded up front.
"""

from __future__ import annotations

import logging
from contextlib import asynccontextmanager
from pathlib import Path

from fastapi import FastAPI
from fastapi.responses import FileResponse
from fastapi.staticfiles import StaticFiles

from metrix_api import __version__, server_log
from metrix_api.config import Config, load_config
from metrix_api.live import Registry
from metrix_api.routes import (
    compare,
    discovery,
    live,
    logs,
    plans,
    profiles,
    recordings,
    schemas,
    series,
)
from metrix_api.store import recordings as store_recordings
from metrix_api.store.db import connect, migrate

log = logging.getLogger(__name__)

WEB = Path(__file__).resolve().parents[2] / "web"


class RevalidatedStatic(StaticFiles):
    """Static files that must be revalidated rather than heuristically cached.

    There is no build step and no content hashing, so an edited module keeps its URL.
    Without an explicit Cache-Control a browser is free to invent a freshness window
    and serve stale code after a reload -- which looks exactly like a change that did
    not work. `no-cache` still allows a 304 via ETag, so this costs a round trip, not
    a re-download.
    """

    def file_response(self, *args, **kwargs):  # type: ignore[override]
        response = super().file_response(*args, **kwargs)
        response.headers["Cache-Control"] = "no-cache"
        return response


def create_app(config: Config | None = None) -> FastAPI:
    settings = (config or load_config()).ensure_layout()
    # Before anything below logs, so startup itself is on the Logs page.
    server_log.install()

    @asynccontextmanager
    async def lifespan(app: FastAPI):
        yield
        # Close every open recording rather than leaving rows stuck in 'running',
        # which would be indistinguishable from one still going.
        await app.state.registry.stop_all()

    app = FastAPI(title="Metrix", version=__version__, lifespan=lifespan)
    app.state.config = settings
    # Live recordings are running tasks, so they live in memory. What survives a
    # restart is in SQLite, which is why the recorder writes as it goes.
    app.state.registry = Registry()

    # Bring the database up to date once, at startup, rather than per request.
    conn = connect(settings.database)
    try:
        migrate(conn)
        # Nothing can still be running: live recordings are tasks in memory, and this
        # process has only just started.
        if abandoned := store_recordings.abandon_running(conn):
            log.warning("closed %d recording(s) left running by a previous process", len(abandoned))
    finally:
        conn.close()

    @app.get("/api/health")
    async def health() -> dict[str, str]:
        # `home` and why it was chosen: the front end shows both, because the first
        # question about a missing recording is which directory it was written to.
        return {
            "status": "ok",
            "version": __version__,
            "home": str(settings.home),
            "home_source": settings.home_explanation,
            "database": str(settings.database),
        }

    app.include_router(profiles.router)
    app.include_router(schemas.router)
    app.include_router(plans.router)
    app.include_router(discovery.router)
    # Live routes first: /recordings/live must not be read as a recording id.
    app.include_router(live.router)
    app.include_router(recordings.router)
    app.include_router(series.router)
    app.include_router(compare.router)
    app.include_router(logs.router)

    if WEB.is_dir():
        # Mounted last so /api/* wins. The page is a single document; deep links are
        # hash routes, so there is no server-side routing to do.
        app.mount("/", RevalidatedStatic(directory=WEB, html=True), name="web")
    else:  # pragma: no cover - only when running from an unusual layout

        @app.get("/")
        async def missing_web() -> FileResponse | dict[str, str]:
            return {"error": f"static files not found at {WEB}"}

    return app


app = create_app()
