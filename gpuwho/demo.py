"""`gpuwho demo`: the live dashboard on a made-up cluster, to try gpuwho without any servers.

The cluster lives in demo.json (also the test fixture and the README screenshot). Each
poll nudges the numbers, and a few scripted events play out: a job finishes, another
one starts, so the screen looks like a real lab.
"""
import copy
import json
import os
import random
import time

DATA = os.path.join(os.path.dirname(os.path.abspath(__file__)), "demo.json")


def load():
    with open(DATA) as f:
        return json.load(f)["servers"]


def clamp(x, lo, hi):
    return max(lo, min(hi, x))


# (poll number, server, gpu index, what happens)
EVENTS = [
    (4, "node01", 2, "finish"),
    (7, "node03", 3, ("start", "frank", 61200.0, "python train_lora.py --model qwen2.5-7b --bf16")),
    (11, "node01", 2, ("start", "alice", 40900.0, "python train.py --config configs/vit_l16.yaml --batch-size 256")),
    (14, "node03", 3, "finish"),
]


class DemoServer:
    """Looks like remote.Server to the UI; every probe moves the numbers a little."""

    def __init__(self, entry, seed=0):
        self.cfg = {"name": entry["name"], "host": entry["host"]}
        self.name = entry["name"]
        self.data = copy.deepcopy(entry["data"])
        self.error = None
        self.updated = None
        self.latency = None
        self.version = 0
        self.blocked = False
        self.retry_at = 0.0
        self.polls = 0
        self.rng = random.Random(seed)

    def due(self):
        return True

    def retry(self):
        pass

    def close(self):
        pass

    def _event(self, gpu, what):
        if what == "finish":
            gpu["procs"] = [p for p in gpu["procs"] if p.get("type") == "G"]
            gpu["mem_used"], gpu["util"] = 4.0, 0
        else:
            _, user, mem, cmd = what
            pid = self.rng.randint(100000, 999999)
            gpu["procs"] = [p for p in gpu["procs"] if p.get("type") == "G"] + [
                {"pid": pid, "mem": mem, "type": "C", "user": user, "cpu": 100.0, "container": None,
                 "cmd": cmd, "rss_kb": 6 * 2 ** 20}]
            gpu["mem_used"], gpu["util"] = mem + 200, 90

    def probe(self):
        rng, d = self.rng, self.data
        self.polls += 1
        for n, server, index, what in EVENTS:
            if n == self.polls % 16 and server == self.name:
                self._event(d["gpus"][index], what)
        for g in d["gpus"]:
            if g.get("invalid"):
                continue
            busy = any(p.get("type") != "G" for p in g["procs"])
            if busy:
                g["util"] = clamp(g["util"] + rng.randint(-7, 7), 55, 100)
            else:
                g["util"] = 0
            g["temp"] = clamp(round(0.8 * g["temp"] + 0.2 * (32 + 0.5 * g["util"]) + rng.uniform(-1, 1)), 28, 87)
            if g.get("fan") is not None:
                g["fan"] = clamp(30 + (g["temp"] - 45) * 2, 30, 100)
            if g.get("power") is not None:
                idle = 0.08 * g["power_limit"]
                g["power"] = round(idle + (g["power_limit"] - idle) * g["util"] / 100 * rng.uniform(0.85, 1.0), 1)
        cpu = d["cpu"]
        cpu["usage"] = clamp(round(cpu["usage"] + rng.uniform(-4, 4), 1), 1, 99)
        cpu["load"] = [round(clamp(x + rng.uniform(-0.6, 0.6), 0, cpu["count"]), 1) for x in cpu["load"]]
        d["time"] = time.time()
        self.updated = time.time()
        self.latency = rng.uniform(0.25, 0.6)
        self.version += 1
        return self


def servers():
    return [DemoServer(entry, seed=i) for i, entry in enumerate(load())]
