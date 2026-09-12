"""Shared request dependencies.

The configuration is resolved once at startup and handed down; nothing below this
reads the environment. Database connections are per-request because sqlite3 objects
belong to the thread that made them, and FastAPI runs sync endpoints in a pool.
"""

from __future__ import annotations

import sqlite3
from collections.abc import Iterator

from fastapi import Depends, Request

from metrix_api.config import Config
from metrix_api.store.db import connect


def get_config(request: Request) -> Config:
    return request.app.state.config


def get_db(config: Config = Depends(get_config)) -> Iterator[sqlite3.Connection]:
    conn = connect(config.database)
    try:
        yield conn
    finally:
        conn.close()
