// The map comes from the authoritative directory. Reception is clocked separately
// in this process after a full datagram of visible positions has been decoded.
export function receive(packet, client, players) {
  if (
    packet.length < 32 ||
    packet.toString('ascii', 0, 4) !== 'MSO1' ||
    packet.readUInt16LE(12) !== client.id
  )
    return false;
  const records = packet.readUInt16LE(14);
  if (!records || records > players || packet.length !== 16 + records * 16) return false;
  const tick = Number(packet.readBigUInt64LE(4));
  if (!Number.isSafeInteger(tick) || tick <= client.tick) return false;
  let own = null;
  const members = new Set();
  for (let offset = 16; offset < packet.length; offset += 16) {
    const object = packet.readUInt32LE(offset);
    if (!object || object > players || members.has(object)) return false;
    members.add(object);
    const x = packet.readInt32LE(offset + 4),
      y = packet.readInt32LE(offset + 8),
      z = packet.readInt32LE(offset + 12);
    if (object === client.id) own = [x / 1000, y / 1000, z / 1000];
  }
  if (!own) return false;
  client.own = own;
  members.delete(client.id);
  client.neighbors = [...members];
  client.tick = tick;
  client.frames++;
  client.bytes += packet.length;
  return true;
}

const distinct = (values) => [...new Set(values)].sort((a, b) => a - b);
export function clusterEvents(previous, current) {
  if (!previous.length) return [{ kind: 'start', clusters: distinct(current.map((p) => p.cluster)) }];
  const old = new Map(previous.map((p) => [p.id, p.cluster]));
  const into = new Map(),
    from = new Map();
  for (const player of current) {
    const before = old.get(player.id);
    if (before === undefined) continue;
    if (!into.has(player.cluster)) into.set(player.cluster, new Set());
    if (!from.has(before)) from.set(before, new Set());
    into.get(player.cluster).add(before);
    from.get(before).add(player.cluster);
  }
  const events = [];
  for (const [id, sources] of into) {
    if (sources.size > 1) events.push({ kind: 'merge', before: distinct(sources), after: [id] });
  }
  for (const [id, targets] of from) {
    if (targets.size > 1) events.push({ kind: 'split', before: [id], after: distinct(targets) });
  }
  return events;
}

export class Telemetry {
  window = [];
  history = [];
  events = [];
  previous = [];
  travelerIsland = null;

  sample(frame, clients, now = performance.now()) {
    const record = {
      now,
      workers: frame.workers.map((w) => ({ tick: w.tick, seconds: w.seconds })),
      applied: frame.peerApplied,
      counts: clients.map((c) => c.frames),
    };
    this.window.push(record);
    while (this.window.length > 1 && this.window[1].now <= now - 2000) this.window.shift();
    const first = this.window[0];
    const elapsed = (now - first.now) / 1000;
    const ready = elapsed >= 0.75;
    const workers = frame.workers.map((w, i) => {
      const past = first.workers[i];
      const time = w.seconds - past.seconds;
      return { ...w, hz: ready ? (time > 0 ? (w.tick - past.tick) / time : 0) : null };
    });
    const serverHz = ready ? Math.min(...workers.map((w) => w.hz)) : null;
    const peerHz = ready ? (frame.peerApplied - first.applied) / elapsed : null;
    const players = frame.players.map((player) => {
      const i = player.id - 1;
      return {
        ...player,
        hz: ready ? (clients[i].frames - first.counts[i]) / elapsed : null,
        rxTick: clients[i].tick,
        rxPosition: clients[i].own,
        neighbors: clients[i].neighbors,
      };
    });
    const rates = players
      .map((p) => p.hz)
      .filter((hz) => hz !== null)
      .sort((a, b) => a - b);
    const medianHz = rates.length
      ? (rates[Math.floor((rates.length - 1) / 2)] + rates[Math.floor(rates.length / 2)]) / 2
      : null;
    const minHz = rates[0] ?? null;
    if (ready) this.history.push({ t: frame.seconds, serverHz, medianHz, minHz });
    this.history = this.history.filter((r) => r.t >= frame.seconds - 60);
    const additions = clusterEvents(this.previous, players);
    const handoff = players.find((p) => p.id === frame.handoffPlayer);
    const old = this.previous.find((p) => p.id === frame.handoffPlayer);
    if (handoff && old && old.worker !== handoff.worker)
      additions.unshift({
        kind: 'handoff',
        player: handoff.id,
        after: [handoff.cluster],
        source: old.worker,
        target: handoff.worker,
      });
    const traveler = players.find((p) => p.id === frame.traveler);
    if (traveler && players.some((p) => p.id !== traveler.id && p.cluster === traveler.cluster)) {
      if (this.travelerIsland !== null && traveler.cluster !== this.travelerIsland) {
        additions.unshift({
          kind: 'move',
          player: traveler.id,
          before: [this.travelerIsland],
          after: [traveler.cluster],
        });
      }
      this.travelerIsland = traveler.cluster;
    }
    this.events.unshift(
      ...additions.map((e) => ({
        ...e,
        t: frame.seconds,
        workers: distinct(
          players.filter((p) => (e.after ?? e.clusters).includes(p.cluster)).map((p) => p.worker),
        ),
      })),
    );
    this.events.length = Math.min(this.events.length, 30);
    this.previous = players;
    return {
      ...frame,
      players,
      workers,
      serverHz,
      medianHz,
      minHz,
      peerHz,
      history: this.history,
      events: this.events,
    };
  }
}
