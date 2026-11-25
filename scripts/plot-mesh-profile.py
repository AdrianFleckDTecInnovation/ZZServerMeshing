"""Render the measured mesh profile. Requires Python 3 and Matplotlib."""

import argparse
import json
from math import ceil
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.ticker import FuncFormatter

ROOT = Path(__file__).resolve().parents[1]
TEAL, BLUE, ORANGE, GREY = "#006f68", "#365ca2", "#b94e16", "#526477"
SYSTEMS = {"windows": ("Windows", "-"), "linux": ("Linux", "--")}


def panels(title, rows=2):
    fig, axes = plt.subplots(rows, 2, figsize=(12.4, 3.85 * rows + .45),
                             layout="constrained", squeeze=False)
    fig.suptitle(title, x=.01, ha="left", fontsize=19, fontweight="bold", color="#172d40")
    for ax in axes.flat:
        ax.spines[["top", "right"]].set_visible(False)
        ax.spines[["left", "bottom"]].set_color("#aab7c1")
        ax.grid(axis="y", color="#e1e7eb")
        ax.set_axisbelow(True)
        ax.tick_params(colors="#344b5c", labelsize=9)
    return fig, axes.flat


def label(ax, title, x, y):
    ax.set_title(title, loc="left", fontsize=12, fontweight="bold")
    ax.set_xlabel(x)
    ax.set_ylabel(y)


def line(ax, x, y, name, color, style="-", marker="o"):
    ax.plot(x, y, style, marker=marker, color=color, linewidth=2, markersize=4, label=name)


def powers(ax, values):
    ax.set_xscale("log", base=2)
    ax.set_xticks(sorted(set(values)))
    ax.xaxis.set_major_formatter(FuncFormatter(lambda v, _: f"{v:g}"))


def legend(ax):
    ax.legend(frameon=False, fontsize=8, ncols=2)


def draw_space(data, out, selected):
    fig, (scale, traffic, merge, switch) = panels("Spieler im Raum")
    sizes = set()
    for os, (name, style) in SYSTEMS.items():
        spatial = data["quick"][os]["spatial"]
        rows = spatial["matched_layouts"]
        x = [r["players"] for r in rows]
        sizes.update(x)
        for key, tag, color in [("separate", "4 Inseln", TEAL), ("dense", "1 Insel", ORANGE)]:
            line(scale, x, [r[key]["step_with_topology_ms"]["p95"] for r in rows],
                 f"{tag} · {name}", color, style)
        outbound = [r for r in spatial["exchange"] if r["phase"] == "exchange"]
        line(switch, [r["players_switching"] for r in outbound],
             [r["step_with_topology_ms"]["p95"] for r in outbound], name,
             TEAL if os == "windows" else BLUE, style)
    scale.axhline(50, color=GREY, linestyle=":", label="20-Hz-Budget")
    powers(scale, sizes)
    scale.set_yscale("log", base=2)
    scale.set_yticks([.5, 1, 2, 4, 8, 16, 32, 50, 128])
    scale.yaxis.set_major_formatter(FuncFormatter(lambda v, _: f"{v:g}"))
    label(scale, "Gleiche Spieler und acht Worker", "Gleichzeitig bewegte Spielfiguren", "Schritt inkl. Inselabfrage · p95 (ms)")
    legend(scale)
    spatial = data["quick"][selected]["spatial"]
    rows = spatial["independent"]
    x = [r["end_islands"] for r in rows]
    for field, name, color in [("frames", "alle Worker", BLUE), ("first_worker_frames", "an Worker 1", TEAL),
                               ("foreign_home_frames", "zwischen Inseln", ORANGE)]:
        line(traffic, x, [r[field] / r["ticks"] for r in rows], name, color)
    traffic.set_xticks(x)
    label(traffic, "16 Spieler und zwei Worker je Insel", "Räumlich getrennte Inseln", "Zugestellte Entity-Frames / Schritt")
    legend(traffic)
    holds = [r for r in spatial["merge_split"] if r["phase"] == "hold"]
    positions = list(range(len(holds)))
    buckets = [r["frames"] / r["ticks"] for r in holds]
    merge.bar(positions, buckets, color=[ORANGE if r["end_islands"] == 1 else TEAL for r in holds], width=.68)
    merge.bar_label(merge.containers[0], fmt="%g", fontsize=9, padding=2)
    merge.set_xticks(positions, [r["end_islands"] for r in holds])
    merge.margins(y=.15)
    label(merge, "Vier Gruppen verschmelzen und trennen sich", "Verbleibende Inseln in zeitlicher Reihenfolge", "Entity-Frames / Schritt · 64 Spieler")
    powers(switch, [4, 8, 16, 32, 64])
    label(switch, "Zwei Inseln tauschen Spieler aus", "Wechselnde Spielfiguren von insgesamt 64", "Schritt inkl. Inselabfrage · p95 (ms)")
    legend(switch)
    fig.savefig(out / "mesh-spielercluster.png", dpi=155)
    plt.close(fig)


def draw_latency(data, out, selected):
    fig, (echo, rate, ticks, replies, phases, network) = panels("Eingabelatenz, Zustellung und Netzverkehr", rows=3)
    counts = set()
    for os, (name, style) in SYSTEMS.items():
        rows = data["quick"][os]["browser"]["rows"]
        x = [r["clients"] for r in rows]
        counts.update(x)
        line(echo, x, [r["echo_ms"]["p50"] for r in rows], "p50 · "+name, TEAL, style)
        line(echo, x, [r["echo_ms"]["p95"] for r in rows], "p95 · "+name, ORANGE, style)
        line(rate, x, [r["snapshot_MiB_s"] for r in rows], name,
             TEAL if os == "windows" else BLUE, style)
        line(ticks, x, [min(m["tick_hz"] for m in r["server_metrics"]) for r in rows],
             "Server · " + name, TEAL, style)
        line(ticks, x, [r["min_snapshot_hz"] for r in rows], "Client · " + name, ORANGE, style)
        drivers = data["quick"][os]["browser"]["load_drivers"]
        line(replies, x, [100 * r["echoed"] / r["planned"] for r in rows],
             f"{drivers} Decoder-Threads · {name}", TEAL, style)
        single = data["browser_single_driver"][os]["rows"]
        line(replies, [r["clients"] for r in single], [100 * r["echoed"] / r["sent"] for r in single],
             "1 Decoder-Thread · " + name, ORANGE, style)
    echo.set_yscale("log", base=2)
    echo.set_yticks([25, 50, 100, 200, 400, 800, 1600])
    echo.yaxis.set_major_formatter(FuncFormatter(lambda v, _: f"{v:g}"))
    echo.axhline(100, color=GREY, linestyle=":", label="Messziel p95")
    for ax in (echo, rate, ticks, replies):
        powers(ax, counts)
        legend(ax)
    label(echo, "Eigene Eingabe im empfangenen Snapshot", "Echte WebSocket-Verbindungen · 300 NPCs", "Eingabe bis Sequenz-Echo (ms)")
    label(rate, "Snapshots an alle verbundenen Clients", "Echte WebSocket-Verbindungen · 300 NPCs", "Empfangene Snapshot-Nutzlast (MiB/s)")
    ticks.set_ylim(bottom=0)
    replies.margins(y=.2)
    label(ticks, "Langsamster Server und langsamster Client", "Echte WebSocket-Verbindungen", "Server-Ticks bzw. Snapshots pro Sekunde")
    label(replies, "Einfluss des Lastgenerators", "Echte WebSocket-Verbindungen", "Bestätigte Eingaben (% aller geplanten)")
    cases = [(p, key) for p in [64, 256] for key in ["separate", "dense"]]
    rows = {r["players"]: r for r in data["quick"][selected]["spatial"]["matched_layouts"]}
    bottom = [0.0] * len(cases)
    for key, name, color in [("simulate_publish", "Simulation + Versandvorbereitung", TEAL),
                             ("delivery_ack", "Zustellung + ACK", BLUE), ("topology", "Inselabfrage", ORANGE)]:
        values = [rows[p][k]["phase_mean_ms"][key] for p, k in cases]
        phases.bar(range(len(cases)), values, bottom=bottom, label=name, color=color)
        bottom = [a + b for a, b in zip(bottom, values)]
    phases.set_xticks(range(len(cases)), [f"{p}\n{4 if k=='separate' else 1} Insel{'n' if k=='separate' else ''}" for p,k in cases])
    label(phases, "Wo der direkte Bevy-Schritt Zeit kostet",
          f"Spielfiguren und räumliche Verteilung · {SYSTEMS[selected][0]}", "Arithmetisches Mittel (ms / Schritt)")
    legend(phases)
    phases_net = [("isolated","getrennt"), ("approach","Annäherung"), ("depart","Trennung"), ("exchange","Wechsel")]
    for i, (os, (name, _)) in enumerate(SYSTEMS.items()):
        segments = data["wire"][os]["scenes"][-1]["segments"]
        values = []
        for phase, _ in phases_net:
            times = sorted(t for s in segments if s["phase"] == phase for t in s["convergence_ms"])
            values.append(times[ceil(.95 * len(times))-1])
        network.bar([v + (i-.5)*.36 for v in range(len(values))], values, width=.34,
                    color=TEAL if i==0 else BLUE, label=name)
    network.set_xticks(range(len(phases_net)), [s for _,s in phases_net])
    label(network, "Bis alle acht mTLS-Prozesse die Sicht haben", "Je Phase zusammengefasste Messschritte", "Konvergenz auf Loopback · p95 (ms)")
    legend(network)
    fig.savefig(out / "mesh-latenz.png", dpi=155)
    plt.close(fig)


def draw_entities(data, out, selected):
    fig, (npc, entities, workers, candidates) = panels("NPCs, Weltobjekte und räumlicher Suchaufwand")
    service = data["browser_comparison"]["summary"]
    for variant, name, color in [("reference","Original", GREY), ("candidate","Integrationsstand", TEAL)]:
        rows = [r for r in service if r["variant"] == variant]
        for k, style in [("p95","-"),("p99","--")]:
            line(npc, range(len(rows)), [r[f"slowest_{k}_ms"] for r in rows], f"{name} · {k}", color, style)
    npc.set_xticks(range(len(rows)), [str(r["npcs"]) for r in rows])
    label(npc, "Bewegte NPCs auf drei Sim-Servern", "NPCs insgesamt · drei WebSocket-Clients · Windows", "Langsamster Server · Tickzeit (ms)")
    legend(npc)
    for os, (name, style) in SYSTEMS.items():
        rows = [r for r in data["library"][os]["scenes"] if r["physical_islands"] == r["workers"]]
        line(entities, range(len(rows)), [r["loop_ns"]["p95"]/1e6 for r in rows],
             name, TEAL if os == "windows" else BLUE, style)
    entities.set_xticks(range(len(rows)), [f"{r['objects']}\n{r['workers']} W." for r in rows])
    label(entities, "256 Positionsobjekte pro unabhängiger Insel", "Weltobjekte und Worker · vier Änderungen je Insel/Schritt", "Serieller Poolschritt · p95 (ms)")
    legend(entities)
    for os, (name, style) in SYSTEMS.items():
        rows = data["quick"][os]["spatial"]["shared_workers"]
        x = [r["workers"] for r in rows]
        line(workers, x, [r["step_with_topology_ms"]["p95"] for r in rows], name,
             TEAL if os == "windows" else BLUE, style)
    workers.set_xticks(x)
    workers.set_ylim(bottom=0)
    label(workers, "Gleiche Last auf mehr seriell getakteten Workern",
          "Worker für dieselben 64 Spielfiguren auf vier Inseln", "Schritt inkl. Inselabfrage · p95 (ms)")
    legend(workers)
    rows = data["quick"][selected]["spatial"]["matched_layouts"]
    x = [r["players"] for r in rows]
    for key, tag, color in [("separate", "4 Inseln", TEAL), ("dense", "1 Insel", ORANGE)]:
        values = [r[key]["spatial_candidates"] / r[key]["ticks"] for r in rows]
        line(candidates, x, values, tag, color)
    powers(candidates, x)
    candidates.set_yscale("log", base=2)
    candidates.yaxis.set_major_formatter(FuncFormatter(lambda n, _: f"{int(n):,}".replace(",", " ")))
    label(candidates, "Wie viele Sichtbereiche der Index findet",
          "Bewegte Spielfiguren · acht Worker", "Geprüfte räumliche Kandidaten / Schritt")
    legend(candidates)
    fig.savefig(out / "mesh-objekte.png", dpi=155)
    plt.close(fig)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--data", type=Path, default=ROOT / "docs/benchmarks/mesh-profile-2026-11-25.json")
    parser.add_argument("--quick", type=Path, help="Replace this platform's spatial and input measurements")
    parser.add_argument("--output", type=Path, default=ROOT / "target/mesh-profile")
    args = parser.parse_args()
    data = json.loads(args.data.read_text(encoding="utf8"))
    system = "windows"
    if args.quick:
        fresh = json.loads(args.quick.read_text(encoding="utf8"))
        system = fresh["spatial"]["os"]
        if system not in SYSTEMS:
            parser.error("expected a Windows or Linux quick profile")
        data["quick"][system] = fresh
    args.output.mkdir(parents=True, exist_ok=True)
    for draw in [draw_space, draw_latency, draw_entities]:
        draw(data, args.output, system)
    print(args.output)
