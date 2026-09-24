"""Client keys, loaded once per process rather than once per connection.

AsyncSSH loads every client key from disk each time it builds a connection's options,
and parsing a private key is CPU work -- tens of milliseconds for an RSA key on a
fast machine, seconds on a slow one. It runs in a thread, but under the GIL, so
probes started together load their keys one after another: four probes paid four
loads in series, and a group verification timed out while still building options.

So the two client-key loaders are memoised. A cached load is reused only while the
directories its files came from are unchanged -- any file in them added, removed or
modified (a key, its `.pub`, its `-cert.pub`) loads again. Concurrent first callers
wait for the one load in progress instead of each starting their own. Each caller
gets its own copies of the key pairs, because a connection records the signature
algorithm it negotiated on the key pair it signs with.

Loads involving a passphrase, certificates passed in, or anything other than paths
are not cached; they are passed straight through.
"""

from __future__ import annotations

import copy
import os
import threading
from pathlib import Path

_cache: dict[object, tuple[object, list]] = {}
_locks: dict[object, threading.Lock] = {}
_guard = threading.Lock()


def share(connection_module: object) -> None:
    """Memoise AsyncSSH's client-key loaders as `connection` calls them, once."""
    if getattr(connection_module, "_metrix_shared_keys", False):
        return
    for name, key in (
        ("load_default_keypairs", _default_key),
        ("load_keypairs", _paths_key),
    ):
        original = getattr(connection_module, name, None)
        if callable(original):
            setattr(connection_module, name, _memoised(original, key))
    connection_module._metrix_shared_keys = True  # type: ignore[attr-defined]


def clear() -> None:
    with _guard:
        _cache.clear()
        _locks.clear()


def _memoised(original, cache_key):
    def loader(*args, **kwargs):
        found = cache_key(args, kwargs)
        if found is None:
            return original(*args, **kwargs)
        identity, directories = found
        state = _fingerprint(directories)
        with _guard:
            lock = _locks.setdefault(identity, threading.Lock())
        with lock:
            cached = _cache.get(identity)
            if cached is None or cached[0] != state:
                cached = (state, list(original(*args, **kwargs)))
                _cache[identity] = cached
        return [copy.copy(pair) for pair in cached[1]]

    loader.__wrapped__ = original  # type: ignore[attr-defined]
    return loader


def _default_key(args: tuple, kwargs: dict):
    """`load_default_keypairs(passphrase=None, certlist=())`: the files in ~/.ssh."""
    passphrase = args[0] if args else kwargs.get("passphrase")
    certlist = args[1] if len(args) > 1 else kwargs.get("certlist", ())
    if passphrase is not None or certlist:
        return None
    return ("default",), [Path("~", ".ssh").expanduser()]


def _paths_key(args: tuple, kwargs: dict):
    """`load_keypairs(keylist, passphrase, certlist, ...)`, when keylist is paths."""
    keylist = args[0] if args else kwargs.get("keylist")
    passphrase = args[1] if len(args) > 1 else kwargs.get("passphrase")
    certlist = args[2] if len(args) > 2 else kwargs.get("certlist", ())
    if passphrase is not None or certlist:
        return None
    items = keylist if isinstance(keylist, (list, tuple)) else [keylist]
    if not items or not all(isinstance(item, (str, os.PathLike)) for item in items):
        return None
    paths = [Path(item).expanduser() for item in items]
    # Everything else that shapes the result, in a hashable form.
    rest = tuple(repr(a) for a in args[3:]) + tuple(
        sorted((k, repr(v)) for k, v in kwargs.items() if k != "loop")
    )
    return ("paths", tuple(str(p) for p in paths), rest), sorted({p.parent for p in paths})


def _fingerprint(directories: list[Path]) -> tuple:
    """Every entry in the directories, with its size and modification time."""
    state = []
    for directory in directories:
        try:
            with os.scandir(directory) as entries:
                for entry in entries:
                    try:
                        info = entry.stat()
                        state.append((str(directory), entry.name, info.st_mtime_ns, info.st_size))
                    except OSError:  # a dangling link, or removed while listing
                        state.append((str(directory), entry.name, None, None))
        except OSError:
            state.append((str(directory), None, None, None))
    return tuple(sorted(state, key=repr))
