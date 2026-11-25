"""Model client egress within spatially separate groups and compare measured tick costs."""

import argparse
import gzip
import json
import math
from pathlib import Path
from statistics import median


DATA = Path(__file__).resolve().parents[1] / "docs/benchmarks/kapazitaet-dichte-windows.json.gz"
PROFILES = ("47 B / 20 Hz", "12 B / 20 Hz (Entwurf)", "20 / 5 / 1 Hz (Entwurf)")


def batches(players, size):
    """(Number of groups, players per group), including the last smaller group."""
    count, remainder = divmod(players, size)
    regions = []
    if count:
        regions.append((count, size))
    if remainder:
        regions.append((1, remainder))
    return regions


def parse_regions(text):
    regions = []
    for entry in text.lower().split(","):
        try:
            count, size = map(int, entry.strip().split("x"))
        except ValueError:
            raise argparse.ArgumentTypeError("use COUNTxSIZE, e.g. 4500x2,1x1000") from None
        if min(count, size) < 1:
            raise argparse.ArgumentTypeError("region counts and sizes must be positive")
        regions.append((count, size))
    return regions


def per_client(others):
    """(Updates/s, bytes/s) for the player's actual moving neighbours."""
    high = min(64, others)
    medium = min(256, others - high)
    low = others - high - medium
    return (
        (others * 20, others * 20 * 47),
        (others * 20, others * 20 * 12),
        (high * 20 + medium * 5 + low, high * 20 * 24 + medium * 5 * 16 + low * 8),
    )


def regional_traffic(regions):
    """Aggregate disjoint groups. Within each group all members see each other."""
    totals = []
    for index in range(len(PROFILES)):
        updates = total_bytes = peak_per_client = 0
        for count, size in regions:
            frequency, byte_rate = per_client(size - 1)[index]
            updates += count * size * frequency
            total_bytes += count * size * byte_rate
            peak_per_client = max(peak_per_client, byte_rate)
        totals.append((updates, total_bytes, peak_per_client))
    return totals


def samples(rows, players, phase):
    return [r[phase]["step_with_topology_ms"]["p95"] for r in rows if r["players"] == players]


def observed_sockets(axis, runs, x_values, model, xlabel, title):
    positions = sorted(set(x_values))
    for x in positions:
        values = [r["client_payload_mbit_s"] for r, n in zip(runs, x_values) if n == x]
        axis.vlines(x, min(values), max(values), lw=2, color="#12766b")
        axis.scatter(x, median(values), color="#12766b", zorder=3,
                     label="Windows · zwei Läufe" if x == positions[0] else None)
    axis.plot(*model, ls="--", color="#526273", label="Modell inkl. Eigenzustand und Header")
    axis.set(title=title, xlabel=xlabel, ylabel="Bei Clients empfangen (Mbit/s)")
    axis.spines[["top", "right"]].set_visible(False)
    axis.grid(axis="y", alpha=.2)


def figure(path, report, players):
    import matplotlib.pyplot as plt
    from matplotlib.ticker import FuncFormatter

    rows = report["runs"]
    fig, ((population, density), (network, timing)) = plt.subplots(2, 2, figsize=(13.4, 9.1))
    fig.subplots_adjust(left=.075, right=.975, bottom=.14, top=.895, wspace=.27, hspace=.44)
    fmt = lambda n: f"{n:,}".replace(",", ".")
    fig.suptitle("Regionen, Spieler und Netzwerkbedarf", weight="bold", size=18, y=.97)
    sockets = report["socket_runs"]
    pair_runs = [r for r in sockets if r["players"] == r["islands"] * 2] + report["pair_population_runs"]
    with gzip.open(DATA.with_name("tickprofil-10000.json.gz"), "rt", encoding="utf-8") as stream:
        debian = json.load(stream)["runs"]["linux24-fullrate-4"]
    ns = sorted({r["players"] for r in pair_runs} | {debian["players"]})
    observed_sockets(population, pair_runs, [r["players"] for r in pair_runs],
                     (ns, [n * (47 * 2 + 27) * 20 * 8 / 1e6 for n in ns]),
                     "Gleichzeitige Spieler", "A   Wachsende Welt, zwei Spieler je Gruppe")
    population.scatter(debian["players"], debian["client_payload_mbit_s"], marker="D",
                       facecolors="white", edgecolors="#9a5122", zorder=3, label="Debian-VM · zehn Sekunden")
    population.set_xticks(ns, [fmt(n) for n in ns], fontsize=9)
    sizes = sorted({r["players"] // r["islands"] for r in sockets})
    observed_sockets(density, sockets, [r["players"] // r["islands"] for r in sockets],
                     (sizes, [1024 * (47 * size + 27) * 20 * 8 / 1e6 for size in sizes]),
                     "Spieler je Gruppe", "B   Dieselben 1.024 Spieler, dichtere Gruppen")
    density.set(xscale="log", yscale="log")
    density.set_xticks(sizes, [fmt(n) for n in sizes])
    density.set_yticks([20, 100, 500, 1000], ["20", "100", "500", "1.000"])
    for axis in (population, density):
        axis.legend(loc="upper left", frameon=False, fontsize=8.5)
    sizes = sorted({r["players"] for r in rows})
    for phase, label, color in (
        ("separate", "Vier Inseln · Tick-p95", "#4576a4"),
        ("dense", "Eine Insel · Tick-p95", "#12766b"),
        ("merge_step", "Einzelner Verschmelzungstick", "#ac6518"),
    ):
        series = [samples(rows, n, phase) for n in sizes]
        values = [median(runs) for runs in series]
        timing.vlines(sizes, [min(r) for r in series], [max(r) for r in series], color=color, lw=2)
        timing.plot(sizes, values, "o-", color=color, label=label)
    anchor = sizes[-1]
    dense = median(samples(rows, anchor, "dense"))
    if players > anchor:
        for power, style, label in ((1, "--", "Annahme linear"), (2, ":", "Annahme quadratisch")):
            timing.plot([anchor, players], [dense, dense * (players / anchor) ** power],
                        ls=style, lw=2, color="#414d59", label=label)
    timing.axhline(50, color="#8b3d44", lw=1, ls="-.", label="Tickbudget 50 ms")
    timing.set(title="D   Acht Bevy-Worker im seriellen Pool", ylabel="Dauer je Schritt (ms)")
    points = sorted({round(math.exp(math.log(2) + i / 160 * math.log(players / 2)))
                     for i in range(161)} | {n for n in (65, 321, players) if n <= players})
    for index, label in enumerate(PROFILES):
        rates = [regional_traffic(batches(players, size))[index][1] * 8 / 1e9 for size in points]
        network.plot(points, rates, lw=2.3, color=("#487398", "#775188", "#138070")[index],
                     label=label)
    hot = players // 10
    if hot > 2:
        mixed = batches(players - hot, 2) + [(1, hot)]
        rate = regional_traffic(mixed)[0][1] * 8 / 1e9
        network.scatter([hot], [rate], marker="D", s=40, color="#a05519", zorder=3,
                        label="47 B · 10 % in einer Gruppe, übrige in Paaren")
    network.set(title=f"C   {fmt(players)} Spieler, berechnete Nachbarzustände",
                ylabel="Gesamter Ausgang für Nachbarzustände (Gbit/s)")
    format_count = FuncFormatter(lambda v, _: f"{v:,.0f}".replace(",", ".") if v >= 1 else f"{v:g}")
    for axis in (timing, network):
        axis.set(xscale="log", yscale="log", xlabel="Gleichzeitige Spieler")
        axis.yaxis.set_major_formatter(format_count)
        axis.spines[["top", "right"]].set_visible(False)
        axis.grid(axis="y", alpha=.2)
        axis.legend(loc="upper center", bbox_to_anchor=(.5, -.18), frameon=False,
                    ncol=2, fontsize=8.5)
    ticks = sorted(set(sizes + ([players] if players > anchor else [])))
    timing.set_xticks(ticks, [format_count(n) for n in ticks])
    network.set(xlim=(1.75, players * 1.15), xlabel="Spieler je vollständig sichtbarer Gruppe")
    ticks = sorted({n for n in (2, 16, 100, 1_000, players) if n <= players})
    network.set_xticks(ticks, [format_count(n) for n in ticks])
    path.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(path, dpi=150)
    plt.close(fig)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--players", type=int, help="world population (default 10000 or sum of --regions)")
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--group-size", type=int, default=None, help="players per separate group (default 2)")
    group.add_argument("--regions", type=parse_regions, help="group counts and sizes, e.g. 4500x2,1x1000")
    parser.add_argument("--link-gbps", type=float, default=25)
    parser.add_argument("--utilization", type=float, default=.70, help="planned share of link rate")
    parser.add_argument("--overhead", type=float, default=.15, help="extra traffic as share of payload")
    parser.add_argument("--base-kbit-per-client", type=float, default=11.84,
                        help="own state and headers, default (47+27) bytes at 20 Hz")
    parser.add_argument("--figure", type=Path, help="write a chart with Matplotlib")
    args = parser.parse_args()
    players = args.players if args.players is not None else 10_000
    if args.regions:
        players = sum(count * size for count, size in args.regions)
        if args.players is not None and args.players != players:
            parser.error("--players must equal the sum of --regions")
    if (players < 2 or (args.group_size is not None and args.group_size < 1)
            or not math.isfinite(args.link_gbps) or args.link_gbps <= 0
            or not 0 < args.utilization <= 1 or not math.isfinite(args.overhead) or args.overhead < 0
            or not math.isfinite(args.base_kbit_per_client) or args.base_kbit_per_client < 0):
        parser.error("require >=2 players, positive group size and finite link rate, 0 < utilization <= 1 "
                     "and finite, nonnegative overhead and base traffic")
    regions = args.regions or batches(players, args.group_size or 2)
    with gzip.open(DATA, "rt", encoding="utf-8") as stream:
        report = json.load(stream)
    rows = report["runs"]
    print(f"{players} Spieler in {', '.join(f'{n}x{s}' for n, s in regions)}, {args.link_gbps:g} Gbit/s je Link, "
          f"{args.utilization:.0%} Zielauslastung, {args.overhead:.0%} Aufschlag")
    base_gbps = players * args.base_kbit_per_client / 1e6
    print(f"Linkrechnung inkl. {base_gbps:.4f} Gbit/s Eigenzustand und Header. "
          "Die anderen Spalten betreffen nur Nachbarn.")
    print("Profil                  Updates/s gesamt   Gbit/s gesamt   Mbit/s/Client Mittel / Maximum   Links")
    for label, (updates, byte_rate, peak) in zip(PROFILES, regional_traffic(regions)):
        total = byte_rate * 8 / 1e9
        links = math.ceil((total + base_gbps) * (1 + args.overhead) / (args.utilization * args.link_gbps))
        print(f"{label:<24} {updates:>14} {total:>15.3f} "
              f"{total * 1000 / players:>17.4f} / {peak * 8 / 1e6:<9.4f} {links:>7}")
    anchor = max(r["players"] for r in rows)
    largest = max(size for _, size in regions)
    if largest >= anchor:
        ms = median(samples(rows, anchor, "dense"))
        ratio = largest / anchor
        print(f"Dichter Tick-p95 bei {anchor} Spielern gemessen: {ms:.2f} ms. "
              f"Rechenbeispiele fuer die Einzelgruppe mit {largest} Spielern: linear {ms * ratio:.1f} ms, "
              f"quadratisch {ms * ratio**2:.1f} ms.")
    if args.figure:
        figure(args.figure, report, players)


if __name__ == "__main__":
    main()
