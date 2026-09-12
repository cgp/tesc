"""FastAPI application factory.

Serves JSON and the static page, and nothing else. There is no templating: the front
end is one HTML file and a handful of ES modules, loaded up front.
"""

from __future__ import annotations

from pathlib import Path

from fastapi import FastAPI
from fastapi.responses import FileResponse
from fastapi.staticfiles import StaticFiles

from metrix_api import __version__
from metrix_api.config import Config, load_config
from metrix_api.routes import profiles, recordings
from metrix_api.store.db import connect, migrate

WEB = Path(__file__).resolve().parents[2] / "web"


def create_app(config: Config | None = None) -> FastAPI:
    settings = (config or load_config()).ensure_layout()

    app = FastAPI(title="Metrix", version=__version__)
    app.state.config = settings

    # Bring the database up to date once, at startup, rather than per request.
    conn = connect(settings.database)
    try:
        migrate(conn)
    finally:
        conn.close()

    @app.get("/api/health")
    async def health() -> dict[str, str]:
        return {"status": "ok", "version": __version__, "home": str(settings.home)}

    app.include_router(profiles.router)
    app.include_router(recordings.router)

    if WEB.is_dir():
        # Mounted last so /api/* wins. The page is a single document; deep links are
        # hash routes, so there is no server-side routing to do.
        app.mount("/", StaticFiles(directory=WEB, html=True), name="web")
    else:  # pragma: no cover - only when running from an unusual layout

        @app.get("/")
        async def missing_web() -> FileResponse | dict[str, str]:
            return {"error": f"static files not found at {WEB}"}

    return app


app = create_app()
