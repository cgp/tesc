"""Client keys are loaded once per process, and again only when their files change."""

from __future__ import annotations

import threading
import time

from metrix_api.observer import ssh_keys


class Pair:
    """Stands in for an AsyncSSH key pair: something a connection writes on."""

    def __init__(self, name: str) -> None:
        self.name = name
        self.sig_algorithm = None


def counting_loader(delay: float = 0.0):
    calls: list[tuple] = []

    def load_keypairs(keylist, passphrase=None, certlist=(), *rest, **kwargs):
        calls.append((keylist, passphrase))
        time.sleep(delay)
        return [Pair(str(path)) for path in keylist]

    return ssh_keys._memoised(load_keypairs, ssh_keys._paths_key), calls


def test_a_key_is_loaded_once_and_again_when_its_directory_changes(tmp_path) -> None:
    key = tmp_path / "id_ed25519"
    key.write_text("first")
    load, calls = counting_loader()

    first = load([str(key)])
    second = load([str(key)])
    assert len(calls) == 1, "the second probe reuses the first load"
    assert first[0] is not second[0], "each caller gets its own copy"

    (tmp_path / "id_ed25519-cert.pub").write_text("a certificate appeared")
    load([str(key)])
    assert len(calls) == 2, "a new file beside the key means load again"

    key.write_text("replaced with a longer key")
    load([str(key)])
    assert len(calls) == 3, "a changed key means load again"


def test_concurrent_first_callers_share_one_load(tmp_path) -> None:
    key = tmp_path / "id_rsa"
    key.write_text("slow to parse")
    load, calls = counting_loader(delay=0.2)
    results: list[list] = []
    threads = [threading.Thread(target=lambda: results.append(load([str(key)]))) for _ in range(4)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    assert len(calls) == 1
    assert len(results) == 4


def test_a_copy_can_be_written_on_without_touching_the_cache(tmp_path) -> None:
    key = tmp_path / "id_rsa"
    key.write_text("rsa")
    load, _ = counting_loader()
    load([str(key)])[0].sig_algorithm = b"rsa-sha2-512"
    assert load([str(key)])[0].sig_algorithm is None


def test_passphrases_and_loaded_keys_pass_straight_through(tmp_path) -> None:
    key = tmp_path / "id_rsa"
    key.write_text("rsa")
    load, calls = counting_loader()
    load([str(key)], "secret")
    load([str(key)], "secret")
    assert len(calls) == 2, "a passphrase-protected load is never cached"
    assert ssh_keys._paths_key(([Pair("loaded")],), {}) is None, "only paths are cached"
