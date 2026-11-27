"""Render the two 10,000-player workloads from the archived measurements."""
import gzip
import json
from pathlib import Path
from statistics import median

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt


folder = Path(__file__).resolve().parents[1] / "docs" / "benchmarks"
with gzip.open(folder / "spielersicht.json.gz", "rt", encoding="utf-8") as file:
    data = json.load(file)

fig, (ticks, reads) = plt.subplots(1, 2, figsize=(11.8, 4.3), layout="constrained")
cases = [(name, length, metrics[batch]) for name, metrics in data["systems"].items()
         for length, batch in [(40, "tick"), (120, "long_tick")]]
for pos, (_, _, measurements) in enumerate(cases):
    for shift, key, color, label in [
        (-0.16, "before", "#536879", "Vor der Sichtfunktion"),
        (0.16, "final", "#158271", "Mit Sichtfunktion"),
    ]:
        samples = [run["tick_ms"]["p95"] for run in measurements[key]]
        x = pos + shift
        ticks.bar(x, median(samples), width=0.3, color=color, label=label if pos == 0 else None)
        ticks.scatter([x] * len(samples), samples, color="#172a37", marker="_", linewidths=1.2, zorder=3)
ticks.axhline(50, color="#a35729", linestyle="--", linewidth=1.2)
ticks.annotate("Budget bei 20 Hz", (0.015, 50), xycoords=("axes fraction", "data"),
               xytext=(0, 4), textcoords="offset points", color="#8c4822", fontsize=8)
ticks.set(xticks=range(len(cases)), xticklabels=[f"{name}\n{length} Ticks" for name, length, _ in cases], ylabel="Tick-p95 pro Lauf (ms)",
          title="Simulation ohne Sichtabruf")
ticks.set_ylim(top=54)
ticks.legend(loc="upper left", bbox_to_anchor=(0, 0.91), frameon=False, fontsize=8)

for pos, (platform, measurements) in enumerate(data["systems"].items()):
    profiles = measurements["sight"]["final"]
    for shift, idx, color, label in [
        (-0.16, 0, "#8da9be", "Isoliert · 0 sichtbare Objekte"),
        (0.16, 1, "#165d81", "Gruppiert · 16 sichtbare Objekte"),
    ]:
        samples = [run["batched_ms"] for report in profiles for run in report["profiles"][idx]["runs"]]
        x = pos + shift
        value = median(samples)
        reads.bar(x, value, width=0.3, color=color, label=label if pos == 0 else None)
        reads.annotate(f"{value:.1f}".replace(".", ","), (x, value), xytext=(0, 3),
                       textcoords="offset points", ha="center", fontsize=9)
reads.set(xticks=[0, 1], xticklabels=list(data["systems"]), ylabel="Zeit für 10.000 Sichten (ms)",
          title="Alle Sichten abrufen · ein Kern")
reads.margins(y=0.22)
reads.legend(frameon=False, fontsize=8)
for axis in (ticks, reads):
    axis.spines[["top", "right"]].set_visible(False)
    axis.set_axisbelow(True)
    axis.grid(axis="y", alpha=0.18)
    axis.set_ylim(bottom=0)
fig.savefig(folder / "spielersicht.png", dpi=170)
