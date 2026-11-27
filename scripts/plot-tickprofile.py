"""Regenerate the tick, socket and worker figures from the archived release measurements."""
import gzip
import json
from pathlib import Path
from statistics import median

import matplotlib.pyplot as plt
import numpy as np


DATA = Path(__file__).resolve().parents[1] / "docs" / "benchmarks"
with gzip.open(DATA / "tickprofil-10000.json.gz", "rt", encoding="utf-8") as stream:
    runs = json.load(stream)["runs"]

BLUE, GREEN, GOLD, INK = "#286492", "#16776e", "#be7623", "#263746"
plt.rcParams.update({"font.size": 10, "axes.spines.top": False, "axes.spines.right": False,
                     "axes.labelcolor": INK, "text.color": INK, "axes.titleweight": "bold",
                     "legend.frameon": False, "svg.fonttype": "none"})


def reps(variant, count):
    return [runs[f"verified-{variant}-{count}-{i}"] for i in (1, 2, 3)]


def configure(ax, title, unit, grid_axis="y"):
    ax.set_title(title, loc="left", pad=12)
    ax.set_ylabel(unit)
    ax.grid(axis=grid_axis, alpha=.18)
    ax.set_axisbelow(True)


def internal():
    fig, (a, b) = plt.subplots(1, 2, figsize=(12.4, 4.8), layout="constrained")
    fig.suptitle("10.000 bewegte Bevy-Spieler · 625 Inseln · 32 Worker", fontsize=16, fontweight="bold")
    counts = [1, 4, 8, 16]
    current = [median(r["tick_ms"]["p95"] for r in reps("candidate", n)) if n in (1, 16)
               else runs[f"verified-activity-100-{n}"]["tick_ms"]["p95"] for n in counts]
    a.plot(counts, current, "o-", color=GREEN, lw=2.3, label="Windows · aktueller Stand")
    for n in (1, 16):
        old = [r["tick_ms"]["p95"] for r in reps("reference", n)]
        new = [r["tick_ms"]["p95"] for r in reps("candidate", n)]
        for values, color in ((old, BLUE), (new, GREEN)):
            a.vlines(n, min(values), max(values), color=color, lw=2.2)
        a.scatter(n, median(old), marker="D", color=BLUE, s=38,
                  label="Windows · vor Optimierung" if n == 1 else None)
    a.scatter([1, 16], [runs[f"linux-current-{n}"]["tick_ms"]["p95"] for n in (1, 16)],
              marker="^", s=50, color=GOLD, label="Debian-VM · aktueller Stand")
    a.axhline(50, color=INK, lw=1, ls="--", label="50 ms bei 20 Hz")
    a.set(xlabel="Parallel ausgeführte Referenzräume", xticks=counts, ylim=(0, None))
    configure(a, "A   Gemeinsame Tickdauer, SQLite im RAM", "p95 der Tickdauer (ms)")
    a.legend(loc="upper center", fontsize=9)
    labels = ["Raumsuche", "Projektion", "Zustellen & ACK", "Eingaben",
              "ECS & Checkpoint", "Routen"]
    phases = ["spatial", "publish", "delivery_ack", "input", "simulation_checkpoint", "routes"]
    pos = np.arange(len(labels))
    for variant, offset, color, label in (("reference", -.18, BLUE, "Vorher"),
                                         ("candidate", .18, GREEN, "Aktuell")):
        values = [median(r["stages"][phase]["ms"]["p50"] for r in reps(variant, 1)) for phase in phases]
        b.barh(pos + offset, values, height=.32, color=color, label=label)
    b.set(yticks=pos, yticklabels=labels, xlabel="p50 der Phase (ms)")
    b.invert_yaxis()
    configure(b, "B   Wo ein serieller Tick Zeit kostet", "", "x")
    b.legend()
    fig.savefig(DATA / "tickprofil-10000.png", dpi=160)
    plt.close(fig)


def sockets():
    rows = sorted((r for key, r in runs.items() if key.startswith("verified-ws-")),
                  key=lambda r: (r["players"], r["islands"], -r["target_snapshot_hz"]))
    systems = ["Win"] * len(rows)
    for key, label in (("linux6-ws-10000", "Deb 6"), ("linux16-ws-16-5000-4-2-1", "Deb 16"),
                       ("linux20-confirm10", "Deb 16 · 10 s"), ("linux24-fullrate-4", "Deb 24 · 10 s")):
        rows.append(runs[key])
        systems.append(label)
    pos = np.arange(len(rows))
    labels = [f'{os}  {r["players"]:,} / {r["islands"]:,} / {r["target_snapshot_hz"]}'.replace(",", ".")
              for os, r in zip(systems, rows)]
    fig, (a, b) = plt.subplots(1, 2, figsize=(12.4, 6.3), layout="constrained", sharey=True)
    fig.suptitle("Echte Spielverbindungen · Windows und Debian-VM · Loopback",
                 fontsize=15, fontweight="bold")
    a.scatter([min(r["server_hz"]) for r in rows], pos - .15, marker="s", color=GREEN, label="Langsamster Server")
    a.scatter([r["receive_hz_min"] for r in rows], pos + .12, marker="o", color=BLUE, label="Langsamster Client")
    a.scatter([r["target_snapshot_hz"] for r in rows], pos + .12, marker="|", s=110, color=INK, label="Client-Sollrate")
    a.set(yticks=pos, yticklabels=labels, xlabel="Ticks bzw. Snapshots je Sekunde", xlim=(-.5, 22))
    a.set_ylim(len(rows) - .5, -.5)
    configure(a, "A   Empfangene Updates", "System · Spieler / Inseln / Soll-Hz", "x")
    a.legend(loc="lower center", bbox_to_anchor=(.5, -.29), ncol=2, fontsize=9)
    ecs = [sum(r["schedule_cpu_cores"]) for r in rows]
    other = [max(0, sum(r["server_cpu_cores"]) - s) for s, r in zip(ecs, rows)]
    drivers = [r["driver_cpu_cores"] for r in rows]
    b.barh(pos, ecs, color=GREEN, label="ECS-Schedule")
    b.barh(pos, other, left=ecs, color=BLUE, label="Übriger Sim-Prozess")
    b.barh(pos, drivers, left=np.add(ecs, other), color=GOLD, label="Lasttreiber")
    b.set_xlabel("CPU-Sekunden je Sekunde über alle Prozesse")
    configure(b, "B   CPU-Aufwand auf demselben Rechner", "", "x")
    b.legend(loc="lower center", bbox_to_anchor=(.5, -.29), ncol=2, fontsize=9)
    fig.savefig(DATA / "socketprofil-10000.png", dpi=160)
    plt.close(fig)


def worker_count():
    counts = [4, 8, 16]
    groups = [[runs[f"linux16-ws-{n}-5000-4-2-{i}"] for i in (1, 2, 3)] for n in counts]
    fig, (a, b) = plt.subplots(1, 2, figsize=(11.8, 4), layout="constrained")
    fig.suptitle("Gleiche Last auf mehr Sim-Servern · 10.000 Verbindungen · Debian, 16 vCPUs",
                 fontsize=14, fontweight="bold")
    for label, color, key in (("Sim-Prozesse", BLUE, "server_cpu_cores"),
                              ("Lasttreiber", GOLD, "driver_cpu_cores")):
        values = [[sum(r[key]) if isinstance(r[key], list) else r[key] for r in g] for g in groups]
        a.plot(counts, [median(v) for v in values], "o-", color=color, lw=2, label=label)
        for x, v in zip(counts, values):
            a.vlines(x, min(v), max(v), color=color, lw=2)
    a.set(xlabel="Anzahl der Sim-Server", xticks=counts, ylim=(0, 6))
    configure(a, "A   CPU-Aufwand", "CPU-Sekunden je Sekunde")
    a.legend()
    times = [[r["input_p95_us"] / 1000 for r in g] for g in groups]
    b.plot(counts, [median(v) for v in times], "o-", color=GREEN, lw=2)
    for x, v in zip(counts, times):
        b.scatter([x] * len(v), v, color=GREEN, s=25)
    b.set(xlabel="Anzahl der Sim-Server", xticks=counts, ylim=(0, 80))
    configure(b, "B   Zeit bis zur bestätigten Eingabe", "p95 pro Lauf (ms)")
    fig.savefig(DATA / "workerprofil-10000.png", dpi=160)
    plt.close(fig)


if __name__ == "__main__":
    internal()
    sockets()
    worker_count()
