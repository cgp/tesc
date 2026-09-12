"""FastAPI application factory.

Stub: F0.1 skeleton only. See docs/implementation-api.md.
"""

from fastapi import FastAPI


def create_app() -> FastAPI:
    app = FastAPI(title="Metrix", version="0.0.0")

    @app.get("/api/health")
    async def health() -> dict[str, str]:
        return {"status": "ok"}

    return app


app = create_app()
