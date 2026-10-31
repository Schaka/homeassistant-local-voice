#!/usr/bin/env python3
"""Keep llama-server's prompt cache (KV cache) across restarts.

Home Assistant sends the same long system prompt (instructions, entity list,
tool definitions) with every request. After a restart the first request must
prefill all of it again: about 35 s for 4.8k tokens on a CPU. llama-server can
write a slot's KV cache to a file and read it back (--slot-save-path), which
takes well under a second.

  llm_slot_cache.py daemon DIR FILE   restore FILE each time llama-server
                                      comes up, save it again when it changed
  llm_slot_cache.py save DIR FILE     save once (entrypoint calls this on
                                      SIGTERM, before it stops llama-server)

DIR must be the --slot-save-path of llama-server. FILE is a bare filename in
it. Saves go to FILE.tmp first, then replace FILE, so a kill in the middle of
a save never leaves a broken cache file.
"""
import json
import os
import sys
import time
import urllib.error
import urllib.request

# llama-server itself, not the guard proxy (entrypoint.sh sets LLM_INTERNAL_URL).
BASE = os.environ.get("LLM_INTERNAL_URL", "http://127.0.0.1:8080")
POLL_S = 5            # health check interval
SAVE_EVERY_S = 300    # how often the daemon looks for a changed slot
MIN_TOKENS = 1024     # smaller slots are side requests (titles), not Assist


def log(msg):
    print(f"[llm-slot-cache] {msg}", flush=True)


def call(path, body=None, timeout=60):
    headers = {"Content-Type": "application/json"}
    key = os.environ.get("LLAMA_API_KEY")
    if key:
        headers["Authorization"] = f"Bearer {key}"
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(BASE + path, data=data, headers=headers,
                                 method="POST" if data else "GET")
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.load(resp)


def healthy():
    try:
        return call("/health", timeout=3).get("status") == "ok"
    except (OSError, ValueError):
        return False


def pick_slot():
    """Most recently used slot that holds an Assist-sized prompt."""
    best = None
    for slot in call("/slots", timeout=10):
        if slot.get("is_processing"):
            return None  # never save while a request runs
        if slot.get("n_prompt_tokens", 0) < MIN_TOKENS:
            continue
        if best is None or slot.get("id_task", -1) > best.get("id_task", -1):
            best = slot
    return best


def save(cache_dir, name, slot=None):
    slot = slot or pick_slot()
    if slot is None:
        return None
    tmp = name + ".tmp"
    res = call(f"/slots/{slot['id']}?action=save", {"filename": tmp}, timeout=120)
    os.replace(os.path.join(cache_dir, tmp), os.path.join(cache_dir, name))
    log(f"saved slot {slot['id']}: {res.get('n_saved')} tokens, "
        f"{res.get('timings', {}).get('save_ms', 0):.0f} ms")
    return slot.get("id_task")


def restore(cache_dir, name):
    if not os.path.isfile(os.path.join(cache_dir, name)):
        log(f"no saved cache {name} yet, the first request prefills the full prompt")
        return
    try:
        res = call("/slots/0?action=restore", {"filename": name}, timeout=120)
        log(f"restored {res.get('n_restored')} tokens into slot 0, "
            f"{res.get('timings', {}).get('restore_ms', 0):.0f} ms")
    except urllib.error.HTTPError as err:
        # Wrong model or context size: the file is useless, start fresh.
        log(f"restore failed ({err.code} {err.read()[:200]!r}), removing {name}")
        os.remove(os.path.join(cache_dir, name))


def daemon(cache_dir, name):
    up = False
    last_task = None
    next_save = 0.0
    while True:
        if not healthy():
            up = False
            time.sleep(POLL_S)
            continue
        if not up:  # first start, or llama-server came back after a crash
            up = True
            last_task = None
            restore(cache_dir, name)
            next_save = time.monotonic() + SAVE_EVERY_S
        elif time.monotonic() >= next_save:
            next_save = time.monotonic() + SAVE_EVERY_S
            try:
                slot = pick_slot()
                if slot is not None and slot.get("id_task") != last_task:
                    last_task = save(cache_dir, name, slot)
            except (OSError, ValueError) as err:
                log(f"save failed: {err}")
        time.sleep(POLL_S)


def main():
    if len(sys.argv) != 4 or sys.argv[1] not in ("daemon", "save"):
        sys.exit(__doc__)
    mode, cache_dir, name = sys.argv[1:]
    if mode == "daemon":
        daemon(cache_dir, name)
    elif healthy():
        try:
            save(cache_dir, name)
        except (OSError, ValueError) as err:
            log(f"save failed: {err}")


if __name__ == "__main__":
    main()
