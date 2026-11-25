"""Render the versioned Infinite Stars benchmarks. Run with Python and matplotlib."""
import json
import gzip
from pathlib import Path
from statistics import median

import matplotlib.pyplot as plt
from matplotlib.ticker import ScalarFormatter

data = Path(__file__).resolve().parents[1] / "docs" / "benchmarks"


def read(name):
    packed = data / (name + ".json.gz")
    if packed.exists():
        with gzip.open(packed, "rt", encoding="utf-8") as f:
            return json.load(f)
    return json.loads((data / (name + ".json")).read_text(encoding="utf-8-sig"))


def curve(runs, key, time):
    costs = {}
    for run in runs:
        for row in run[key]:
            costs.setdefault(row["players"], []).append(time(row))
    sizes = sorted(costs)
    return sizes, [median(costs[n]) for n in sizes]


def matched(ax, runs, value, label, color, style):
    samples = {}
    for run in runs:
        for row in run["matched_layouts"]:
            samples.setdefault(row["players"], []).append(value(row))
    full = sorted(n for n, values in samples.items() if len(values) == len(runs))
    partial = sorted(set(samples) - set(full))
    ax.plot(full, [median(samples[n]) for n in full], style, marker=".", color=color, label=label)
    ax.scatter(partial, [median(samples[n]) for n in partial], facecolors="none", edgecolors=color,
               marker="s" if style == "--" else "o", s=28)


fig, axes = plt.subplots(3, 2, figsize=(12, 11), layout="constrained")
colors = {"windows": "#006887", "debian": "#ac4c19"}
for os, label in [("windows", "Windows"), ("debian", "Debian")]:
    color = colors[os]
    before, after = read(f"octree-before-{os}"), read(f"octree-{os}")
    ax = axes[0, 0]
    for run, name, style in [(before, "vorher", "--"), (after, "Octree", "-")]:
        matched(ax, run, lambda r: r["dense"]["step_with_topology_ms"]["p95"],
                f"{label} {name}", color, style)
    ax = axes[0, 1]
    x, y = curve(after, "island_scaling", lambda r: r["step_with_topology_ms"]["p95"])
    ax.plot(x, y,
            marker="o", color=color, label=label)
    ax = axes[2, 0]
    for phase, name, style in [("merge_step", "Zusammenlauf", "-"), ("split_step", "Trennung", "--")]:
        matched(ax, after, lambda r: r[phase]["step_with_topology_ms"]["max"],
                f"{label} {name}", color, style)
    ax = axes[2, 1]
    fixed = after[0]["shared_workers"]
    workers = [r["workers"] for r in fixed]
    fixed_ms = [median(r["shared_workers"][i]["step_with_topology_ms"]["p95"] for r in after)
                for i in range(len(fixed))]
    ax.plot(workers, fixed_ms, marker="o", color=color, label=label)
    wire = read(f"octree-wire-{os}")["scenes"]
    ax = axes[1, 1]
    for i, scene in enumerate(wire):
        row = scene["segments"][0]
        x = i + (-.19 if os == "windows" else .19)
        peer = (row["tls_tx"] + row["tls_rx"]) / row["barriers"] / 1024
        store = (row["authority_tls_tx"] + row["authority_tls_rx"]) / row["barriers"] / 1024
        ax.bar(x, peer, width=.36, color=color, label=label if i == 0 else None)
        ax.bar(x, store, bottom=peer, width=.36, color=color, hatch="///", alpha=.5)

ax = axes[0, 0]
ax.axhline(50, color="#705959", linewidth=1, label="20-Hz-Budget")
ax.set(xlabel="bewegte Figuren in einer Insel · 8 Worker", ylabel="p95 pro Schritt (ms)",
       title="Dichte Spielgruppe")
ax.set_xscale("log", base=2)
ax.set_xticks([16, 32, 64, 128, 256, 512, 1024])
ax.xaxis.set_major_formatter(ScalarFormatter())
ax = axes[0, 1]
ax.set(xlabel="bewegte Figuren · 32 pro Insel, 2 Worker pro Insel", ylabel="p95 pro Schritt (ms)",
       title="Getrennte Inseln")
ax.set_xscale("log", base=2)
ax.set_xticks([32, 128, 512, 1024])
ax.xaxis.set_major_formatter(ScalarFormatter())
ax = axes[1, 0]
plateaus = [r for r in read("octree-windows")[0]["merge_split"] if r["phase"] == "hold"]
ax.plot(range(len(plateaus)), [r["end_subscriptions"] for r in plateaus], color="#334e74",
        marker="o", label="Objekt-Empfänger-Abos")
ax.plot(range(len(plateaus)), [r["end_pairs"] for r in plateaus], color="#977500",
        marker="s", label="gerichtete Workerpaare")
ax.set_xticks(range(len(plateaus)), [r["end_islands"] for r in plateaus])
ax.set(xlabel="Inseln im Zeitverlauf · 64 Figuren, 8 Worker", ylabel="Anzahl am Ende der Phase",
       title="Gruppen nähern und entfernen sich")
ax = axes[1, 1]
ax.set_xticks(range(3), ["2", "4", "8"])
ax.set(xlabel="Workerprozesse · schraffiert = Authority-Verkehr", ylabel="TLS in beide Richtungen (KiB / Bewegungsschritt)",
       title="Netzverkehr bei getrennten Inseln")
ax = axes[2, 0]
ax.set(xlabel="bewegte Figuren · vier Inseln, acht Worker", ylabel="Dauer des Übergangsticks (ms)",
       title="Alle Gruppen wechseln im selben Tick")
ax.set_xscale("log", base=2)
ax.set_yscale("log")
ax.set_xticks([16, 32, 64, 128, 256, 512, 1024])
ax.xaxis.set_major_formatter(ScalarFormatter())
ax.axhline(50, color="#705959", linewidth=1, label="20-Hz-Budget")
ax = axes[2, 1]
ax.set(xlabel="Worker im direkten Pool · konstant 64 Figuren", ylabel="p95 pro Schritt (ms)",
       title="Dieselbe Last auf einem bis acht Workern")
ax.set_xticks([1, 2, 4, 8])
for ax in axes.flat:
    ax.grid(axis="y", color="#dae1e5", linewidth=.7)
    ax.set_axisbelow(True)
    ax.spines[["top", "right"]].set_visible(False)
    if ax.get_yscale() == "linear":
        ax.set_ylim(bottom=0)
    ax.legend(frameon=False, fontsize=8)
fig.savefig(data / "octree-skalierung.png", dpi=160)

fig, grid = plt.subplots(2, 2, figsize=(12, 7.5), layout="constrained")
axes = grid.ravel()
for os, label in [("windows", "Windows"), ("debian", "Debian")]:
    ax = axes[0]
    legacy = read("octree-browser-original" + ("-debian" if os == "debian" else ""))
    for variant, name, style in [("reference", "Original", "--"), ("candidate", "aktuell", "-")]:
        rows = [r for r in legacy["summary"] if r["variant"] == variant]
        ax.plot([r["npcs"] for r in rows], [r["slowest_p95_ms"] for r in rows],
                style, marker=".", color=colors[os], label=f"{label} {name}")
    ax = axes[1]
    scenes = read("octree-objekte-" + os)["scenes"]
    ax.plot(range(len(scenes)), [r["loop_ns"]["p95"] / 1e6 for r in scenes],
            marker="o", color=colors[os], label=label)
    runs = read("octree-" + os)
    for phase, name, style in [("exchange", "Hinweg", "-"), ("return", "Rückweg", "--")]:
        exchange = {}
        for run in runs:
            for row in run["exchange"]:
                if row["phase"] == phase:
                    exchange.setdefault(row["players_switching"], []).append(row["step_with_topology_ms"]["p95"])
        moving = sorted(exchange)
        axes[3].plot(moving, [median(exchange[n]) for n in moving], style, marker=".",
                     color=colors[os], label=f"{label} {name}")
axes[0].set(xticks=[3000, 9000], xlabel="NPCs · 3 Browser, 3 Server", ylabel="langsamster Server, p95 (ms)",
            title="Ursprüngliche Browser-Demo")
axes[1].set_xticks(range(len(scenes)), [f'{r["objects"]:,} / {r["workers"]}'.replace(",", ".") for r in scenes],
                   rotation=20)
axes[1].set(xlabel="Objekte / Worker · 160 Schritte", ylabel="p95 pro Schritt (ms)",
            title="Selektive Weltänderungen")
client_runs = read("octree-spielclients-windows")["runs"]
players = sorted({r["players"] for r in client_runs})
for sims, color in [(1, "#6c5275"), (3, "#167267")]:
    points = []
    for n in players:
        samples = [r["rows"][0]["echo_ms"]["p95"] for r in client_runs
                   if r["players"] == n and r["servers"] == sims and r["rows"]]
        points.append(median(samples) if samples else float("nan"))
    axes[2].plot(players, points, marker="o", color=color, label=f"{sims} Sim-Server")
axes[2].axhline(100, color="#705959", linewidth=1, label="Echoziel")
axes[2].set_yscale("log")
axes[2].set_yticks([50, 100, 200, 500, 1000, 2000])
axes[2].yaxis.set_major_formatter(ScalarFormatter())
axes[2].set(xticks=players, xlabel="gleichzeitige Browser-Protokollclients", ylabel="Eingabe bis Echo, p95 (ms)",
            title="Spieler am selben Ort · Windows")
axes[3].set(xticks=[4, 8, 16, 32, 64], xlabel="wechselnde Figuren zwischen zwei Inseln",
            ylabel="p95 pro Schritt (ms)", title="Einzelne Spieler wechseln die Gruppe")
for ax in axes:
    ax.set_axisbelow(True)
    ax.grid(axis="y", color="#dae1e5", linewidth=.7)
    ax.spines[["top", "right"]].set_visible(False)
    if ax.get_yscale() == "linear":
        ax.set_ylim(bottom=0)
    ax.legend(frameon=False, fontsize=8)
fig.savefig(data / "octree-spiellast.png", dpi=160)
