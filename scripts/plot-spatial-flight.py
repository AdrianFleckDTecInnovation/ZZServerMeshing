"""Render the spatial flight benchmark from its archived release measurements."""
import gzip
import json
from pathlib import Path
from statistics import median

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.ticker import FuncFormatter

folder = Path(__file__).resolve().parents[1] / "docs" / "benchmarks"
with gzip.open(folder / "raumgruppen.json.gz", "rt", encoding="utf-8") as source:
    data = json.load(source)

fig = plt.figure(figsize=(12.8, 4.7), layout="constrained")
grid = fig.add_gridspec(2, 2, width_ratios=[1, 1.45])
timing = fig.add_subplot(grid[:, 0])
islands = fig.add_subplot(grid[0, 1])
traffic = fig.add_subplot(grid[1, 1], sharex=islands)
blue, teal, orange = "#49658b", "#007369", "#b15321"
y = [3.2, 2.2, 1, 0]
cases = [(os, phase) for os in ("Windows", "Debian") for phase in ("starburst", "regroup")]
for revision, offset, color, label in [("before", .17, blue, "vorher"), ("final", -.17, teal, "mit Regionsindex")]:
    values = []
    for os, phase in cases:
        runs = data["systems"][os]["flight"][revision]
        values.append(median(next(p for p in r["phases"] if p["phase"] == phase)["spatial_ms"]["p50"] for r in runs))
    bars = timing.barh([i + offset for i in y], values, .3, label=label, color=color)
    timing.bar_label(bars, labels=[f"{v:.2f}".replace(".", ",") for v in values], padding=3, fontsize=9)
timing.set_yticks(y, ["Windows · Sternflug", "Windows · Rückflug", "Debian · Sternflug", "Debian · Rückflug"])
timing.set_xlabel("Räumliche Verarbeitung pro Tick (ms)")
timing.xaxis.set_major_formatter(FuncFormatter(lambda v, _: f"{v:.1f}".replace(".", ",")))
timing.set_title("Median aus fünf kurzen Läufen", loc="left")
timing.margins(x=.18, y=.14)
timing.legend(loc="upper center", bbox_to_anchor=(.5, -.15), ncol=2, frameon=False)

trace = data["systems"]["Windows"]["flight"]["final"][0]["trace"]
ticks = range(1, len(trace) + 1)
for axis, column, title, color in [(islands, 0, "Physische Inseln", teal), (traffic, 1, "Positionsupdates an den anderen Worker pro Tick", orange)]:
    axis.step(ticks, [row[column] for row in trace], where="post", color=color, linewidth=1.7)
    axis.set_title(title, loc="left")
    axis.set_ylim(bottom=0)
    for edge in [32, 64, 96, 128]:
        axis.axvline(edge + .5, linewidth=.8, linestyle=":", color="#83929c")
    axis.set_xlim(1, len(trace))
traffic.set_xticks([16, 48, 80, 112, 131], ["Gruppe", "Sternflug", "Rückflug", "Anflug", "Rand"])
traffic.set_xlabel("Aufeinanderfolgende Messticks")
islands.tick_params(labelbottom=False)
for axis in [timing, islands, traffic]:
    axis.spines[["top", "right"]].set_visible(False)
    axis.grid(axis="x" if axis is timing else "y", alpha=.16)
    axis.set_axisbelow(True)
fig.suptitle("Zwei Worker mit je 100 Spielern", x=.01, ha="left", fontsize=17, fontweight="bold")
fig.savefig(folder / "raumgruppen.png", dpi=145)
