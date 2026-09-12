"""Entry point for `uv run metrix-api`."""

import uvicorn


def main() -> None:
    uvicorn.run("metrix_api.main:app", host="127.0.0.1", port=8080, reload=False)


if __name__ == "__main__":
    main()
