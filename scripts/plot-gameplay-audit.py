"""Render the archived per-player ECS measurements in docs/einsatzaudit.md."""
import gzip
import json
from pathlib import Path
from statistics import median

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt

folder = Path(__file__).resolve().parents[1] / "docs" / "benchmarks"
with gzip.open(folder / "einsatzaudit.json.gz", "rt", encoding="utf-8") as source:
    report = json.load(source)
systems = report["systems"]

fig, (curve, comparison) = plt.subplots(1, 2, figsize=(13, 5), layout="constrained")
loads = [0, 64, 256, 1024, 2048]
cases = ["idle128local", "idle128private", "file_base", "file256"]
labels = ["Stillstand · 128 lokal", "Stillstand · 128 privat¹", "Bewegt · Datei · 0", "Bewegt · Datei · 256"]

for system, color, marker, offset in [
    ("Windows", "#156783", "o", -0.18),
    ("Debian-VM", "#a95124", "s", 0.18),
]:
    runs = systems["Debian" if system == "Debian-VM" else system]["ecs"]
    samples = [[r["tick_ms"]["p95"] for r in runs[key]] for key in ("base", "e64", "e256", "e1024", "e2048")]
    curve.fill_between(loads, [min(s) for s in samples], [max(s) for s in samples], color=color, alpha=0.12)
    curve.plot(loads, [median(s) for s in samples], color=color, marker=marker, label=system, linewidth=2)
    for i, case in enumerate(cases):
        values = [r["tick_ms"]["p95"] for r in runs[case]]
        mid = median(values)
        comparison.errorbar(mid, i + offset, xerr=[[mid - min(values)], [max(values) - mid]],
                            color=color, marker=marker, capsize=3, markersize=6, linewidth=1.6)

curve.axhline(50, color="#37434a", linestyle="--", linewidth=1.2, label="50 ms bei 20 Hz")
curve.set(ylim=(0, None), xlabel="Effektprüfungen je bewegter Figur und Tick", ylabel="Dauer des Mesh-Ticks, p95 (ms)")
curve.set_xticks([0, 256, 1024, 2048], ["0", "256", "1.024", "2.048"])
curve.set_title("Zusätzliche Spielregeln im Bevy-ECS", loc="left", fontweight="bold")
curve.legend(loc="upper left", frameon=False)
comparison.axvline(50, color="#37434a", linestyle="--", linewidth=1.2)
comparison.set_yticks(range(len(labels)), labels)
comparison.set(xlim=(0, None), ylim=(3.55, -0.55), xlabel="Dauer des Mesh-Ticks, p95 (ms)")
comparison.set_title("Speicher und Änderungsverfolgung", loc="left", fontweight="bold")
for axis in [curve, comparison]:
    axis.grid(axis="y" if axis is curve else "x", alpha=0.17)
    axis.set_axisbelow(True)
    axis.spines[["top", "right"]].set_visible(False)
fig.suptitle("10.000 Figuren in 625 Inseln auf vier Simulationsthreads", x=0.01, ha="left", fontweight="bold", fontsize=16)
fig.supxlabel("Punkte zeigen den Median der drei Lauf-p95, Flächen und Striche deren Spanne.   ¹ Privater Spielzustand mit Checkpoints im RAM.",
              fontsize=9, color="#37434a")
fig.savefig(folder / "einsatzaudit.png", dpi=145)

fig, (server, client) = plt.subplots(1, 2, figsize=(12.6, 4.0), layout="constrained")
for variant, label, offset, color in [
    ("reference", "Original", -0.13, "#49658b"),
    ("candidate", "Aktuell", 0.13, "#007369"),
]:
    for i, (source, npcs) in enumerate([("default", 3000), ("default", 9000), ("affinity_control", 9000)]):
        runs = report["browser"][source]["runs"]
        values = [max(s["tick_p95_ms"] for s in r["servers"]) for r in runs
                  if r["npcs"] == npcs and r["variant"] == variant]
        mid = median(values)
        server.errorbar(mid, i + offset, xerr=[[mid - min(values)], [max(values) - mid]],
                        fmt="o", capsize=3, color=color, label=label if i == 0 else None)
server.set_yticks(range(3), ["3.000 NPCs", "9.000 NPCs", "9.000 NPCs · feste Kerne"])
server.set(ylim=(2.55, -0.55), xlim=(0, None), xlabel="Tick-p95 des langsamsten Servers (ms)")
server.set_title("Drei Server und drei Snapshot-Empfänger", loc="left", fontweight="bold")
server.legend(frameon=False, loc="lower right")

samples = report["rendered_client"]["samples"]
for i, npcs in enumerate([300, 3000]):
    runs = [s for s in samples if s["npcs"] == npcs]
    for j, r in enumerate(runs):
        client.plot(r["frame_interval_p95_ms"], i + (j - 0.5) * 0.14, marker="D", color="#a95124", ms=6)
    fps = median(s["average_frames_per_second"] for s in runs)
    client.text(max(r["frame_interval_p95_ms"] for r in runs) + 2.3, i, f"Ø {fps:.1f} FPS".replace(".", ","),
                va="center", color="#663c2a")
client.set_yticks([0, 1], ["300 NPCs", "3.000 NPCs"])
client.set(ylim=(1.55, -0.55), xlim=(0, 70), xlabel="Bildintervall des Browser-Hauptthreads, p95 (ms)")
client.set_title("Ein gerenderter Edge-Client, 1.280 × 720", loc="left", fontweight="bold")
for axis in [server, client]:
    axis.grid(axis="x", alpha=0.17)
    axis.set_axisbelow(True)
    axis.spines[["top", "right"]].set_visible(False)
fig.suptitle("Browser-Demo auf Windows", x=0.01, ha="left", fontweight="bold", fontsize=16)
fig.supxlabel("Links Median und Spanne über drei bzw. fünf Läufe. Rechts zwei Zehn-Sekunden-Läufe je Laststufe mit dem aktuellen Client.",
              fontsize=9, color="#37434a")
fig.savefig(folder / "einsatzaudit-browser.png", dpi=145)
