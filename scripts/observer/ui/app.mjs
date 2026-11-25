import { UniverseMap, clusterColor, clusterName, workerColor, playerName, context } from './map.mjs';

const el = (id) => document.getElementById(id);
const format = (value, digits = 1) =>
  value == null
    ? '–'
    : value.toLocaleString('de-DE', {
        minimumFractionDigits: digits,
        maximumFractionDigits: digits,
      });
const clock = (seconds) =>
  String(Math.floor(seconds / 60)).padStart(2, '0') +
  ':' +
  String(Math.floor(seconds % 60)).padStart(2, '0');
const phases = ['Getrennte Inseln', 'Annäherung', 'Verschmelzung', 'Trennung', 'Spielerwechsel'];
const buttons = new Map(),
  workerRows = new Map();
let latest,
  shown,
  paused = false,
  ended = false,
  chosen = 1,
  selectedCluster = null,
  lastEvent = '',
  staleTimer;
const map = new UniverseMap(el('universe'), (id) => {
  chosen = id;
  const p = shown?.players.find((p) => p.id === id);
  if (p && selectedCluster !== null && p.cluster !== selectedCluster) selectedCluster = p.cluster;
  el('player').value = id ?? '';
  if (shown) render(shown);
});

function status(text, state) {
  el('status').textContent = text;
  el('status').className = 'status ' + state;
}
function setLegend() {
  const box = el('mapLegend'),
    mode = el('color').value;
  box.replaceChildren();
  const items =
    mode === 'worker'
      ? (shown?.workers ?? []).map((w) => ['W' + w.id, workerColor(w.id)])
      : mode === 'hz'
        ? [
            ['≥ 17 Hz', '#64d9bc'],
            ['8–17 Hz', '#ecb467'],
            ['< 8 Hz', '#f0848d'],
          ]
        : [];
  for (const [label, color] of items) {
    const span = document.createElement('span'),
      dot = document.createElement('i');
    dot.style.background = color;
    span.append(dot, label);
    box.append(span);
  }
  box.hidden = !items.length;
}
function groupsPanel(players) {
  const counts = new Map(),
    owners = new Map();
  for (const p of players) {
    counts.set(p.cluster, (counts.get(p.cluster) ?? 0) + 1);
    if (!owners.has(p.cluster)) owners.set(p.cluster, new Map());
    const split = owners.get(p.cluster);
    split.set(p.worker, (split.get(p.worker) ?? 0) + 1);
  }
  if (selectedCluster !== null && !counts.has(selectedCluster)) selectedCluster = null;
  for (const [id, button] of buttons)
    if (!counts.has(id)) {
      button.remove();
      buttons.delete(id);
    }
  const ordered = [...counts.keys()].sort((a, b) => a - b);
  for (const [index, id] of ordered.entries()) {
    let button = buttons.get(id);
    if (!button) {
      button = document.createElement('button');
      button.className = 'cluster-row';
      button.style.setProperty('--cluster', clusterColor(id));
      button.innerHTML =
        '<i></i><span class="group-name"><strong></strong><small></small></span><b><span></span></b><output></output>';
      button.querySelector('strong').textContent = 'Cluster ' + String(id).padStart(2, '0');
      button.title = 'Cluster auf der Karte hervorheben';
      button.addEventListener('click', () => {
        selectedCluster = selectedCluster === id ? null : id;
        if (selectedCluster !== null) {
          const members = shown.players.filter((p) => p.cluster === selectedCluster);
          if (!members.some((p) => p.id === chosen)) chosen = members[0].id;
        }
        render(shown);
      });
      buttons.set(id, button);
      el('clusters').insertBefore(button, el('clusters').children[index] ?? null);
    }
    const count = counts.get(id);
    const split = [...owners.get(id)].sort(([a], [b]) => a - b);
    const shares = button.querySelector('small');
    const key = JSON.stringify(split);
    if (shares.dataset.shares !== key) {
      shares.dataset.shares = key;
      shares.replaceChildren(
        ...split.map(([w, n]) => {
          const chip = document.createElement('span');
          chip.style.color = workerColor(w);
          chip.textContent = 'W' + w + ' × ' + n;
          return chip;
        }),
      );
      button.title = split.map(([w, n]) => 'W' + w + ' steuert ' + n + ' Spieler').join(' · ');
    }
    button.lastElementChild.textContent = count;
    button.querySelector('b span').style.width = (count / Math.max(...counts.values())) * 100 + '%';
    button.setAttribute('aria-pressed', selectedCluster === id);
  }
  el('clusterCount').textContent = counts.size;
  map.options.cluster = selectedCluster;
}
function workersPanel(workers) {
  for (const w of workers) {
    let row = workerRows.get(w.id);
    if (!row) {
      row = document.createElement('tr');
      row.innerHTML = '<td><i class="worker-dot"></i></td><td></td><td></td><td></td>';
      row.firstChild.append('W' + String(w.id).padStart(2, '0'));
      row.style.setProperty('--cluster', workerColor(w.id));
      workerRows.set(w.id, row);
      el('workers').append(row);
    }
    row.children[0].title = 'Prozess-ID ' + w.pid;
    row.children[1].textContent = format(w.hz);
    row.children[2].textContent = w.owned;
    row.children[3].textContent = w.replicas;
  }
}
function inspector(players) {
  const pick = el('player');
  if (pick.options.length !== players.length + 1) {
    pick.replaceChildren(new Option('Spieler wählen', ''));
    for (const p of [...players].sort((a, b) => a.id - b.id)) {
      let label = playerName(p.id);
      if (p.id === shown.traveler) label += ' · Inselwechsel';
      if (p.id === shown.handoffPlayer) label += ' · Serverwechsel';
      pick.add(new Option(label, p.id));
    }
  }
  pick.value = chosen ?? '';
  const p = players.find((player) => player.id === chosen);
  const body = el('inspector');
  if (!p) {
    body.textContent = 'Wähle einen Punkt auf der Karte oder einen Spieler aus der Liste.';
    return;
  }
  const details = [
    ['Position X / Y / Z', p.pos.map((v) => format(v)).join(' / ') + ' m'],
    ['Cluster · Authority', clusterName(p.cluster) + ' · W' + String(p.worker).padStart(2, '0')],
    ['Empfang · Abo-Soll', format(p.hz) + ' / ' + 20 / p.period + ' Hz'],
    ['Letzter Empfangstick', p.rxTick ? '#' + p.rxTick : '–'],
    ['Empfangene Nachbarn', String(p.neighbors.length)],
    [
      'Empfangene Position X / Z',
      p.rxPosition ? [p.rxPosition[0], p.rxPosition[2]].map((v) => format(v)).join(' / ') + ' m' : '–',
    ],
  ];
  if (!body.querySelector('dl')) {
    const list = document.createElement('dl');
    list.className = 'details';
    for (const [label] of details) {
      const item = document.createElement('div'),
        term = document.createElement('dt');
      term.textContent = label;
      item.append(term, document.createElement('dd'));
      list.append(item);
    }
    body.replaceChildren(list);
  }
  body.querySelectorAll('dd').forEach((node, i) => {
    node.textContent = details[i][1];
  });
}
function eventsPanel(events) {
  const key = JSON.stringify(events);
  if (key === lastEvent) return;
  lastEvent = key;
  const items = events.slice(0, 6).map((event) => {
    const row = document.createElement('div');
    row.className = 'log-entry';
    const time = document.createElement('time');
    time.textContent = clock(event.t);
    let message;
    if (event.kind === 'start')
      message = 'Test gestartet · ' + event.clusters.length + ' räumliche Cluster';
    else if (event.kind === 'handoff')
      message =
        'Serverübergabe ' + playerName(event.player) + ' · W' + event.source + ' → W' + event.target;
    else {
      const action =
        event.kind === 'move'
          ? 'Inselwechsel ' + playerName(event.player)
          : event.kind === 'merge'
            ? 'Verschmelzung'
            : 'Trennung';
      message = [
        action,
        event.before.map(clusterName).join(' + ') + ' → ' + event.after.map(clusterName).join(' + '),
        event.workers.map((w) => 'W' + w).join(' + '),
      ].join(' · ');
    }
    row.append(time, document.createElement('i'), message);
    return row;
  });
  el('events').replaceChildren(...items);
}
function chart() {
  const { c, w, h } = context(el('chart'));
  if (!w || !h) return;
  const data = shown?.history ?? [],
    now = shown?.seconds ?? 0;
  const hi = Math.max(25, ...data.map((p) => Math.max(p.serverHz, p.medianHz, p.minHz)));
  const plot = { x: 34, y: 10, w: w - 48, h: h - 30 };
  const py = (hz) => plot.y + (1 - hz / hi) * plot.h;
  const px = (t) => plot.x + Math.max(0, Math.min(1, (t - now + 60) / 60)) * plot.w;
  c.font = '9px Consolas, monospace';
  c.lineWidth = 1;
  c.fillStyle = '#92a1b7';
  for (const hz of [0, 10, 20]) {
    const y = py(hz);
    c.strokeStyle = '#2a3342';
    c.beginPath();
    c.moveTo(plot.x, y);
    c.lineTo(w - 13, y);
    c.stroke();
    c.fillText(hz + '', 10, y + 3);
  }
  c.fillText('−60 s', plot.x, h - 4);
  c.fillText('jetzt', w - 43, h - 4);
  for (const [key, color] of [
    ['serverHz', '#b2a0ff'],
    ['medianHz', '#59d3ba'],
    ['minHz', '#edb86b'],
  ]) {
    c.strokeStyle = color;
    c.lineWidth = key === 'serverHz' ? 2.7 : 1.4;
    c.setLineDash(key === 'medianHz' ? [3, 2] : []);
    c.beginPath();
    data.forEach((p, i) => {
      if (i) c.lineTo(px(p.t), py(p[key]));
      else c.moveTo(px(p.t), py(p[key]));
    });
    c.stroke();
    c.setLineDash([]);
  }
}
function render(state) {
  const changeWorkers = shown?.workers.length !== state.workers.length;
  shown = state;
  if (changeWorkers) setLegend();
  el('clock').textContent = clock(state.seconds);
  el('phase').textContent =
    phases[state.phase] + (state.phase === 4 ? ' · ' + playerName(state.traveler) : '');
  [...el('phaseSteps').children].forEach((step, i) => step.classList.toggle('on', i === state.phase));
  el('playerCount').textContent = state.players.length;
  el('workerCount').textContent = state.workers.length + ' mTLS-Serverprozesse';
  el('serverHz').textContent = format(state.serverHz);
  el('clientHz').textContent = format(state.medianHz);
  el('tickCost').textContent = 'Minimum · max. Tickarbeit ' + format(state.stepMs, 2) + ' ms';
  el('slowest').textContent = 'Median · Minimum ' + format(state.minHz) + ' Hz';
  el('tick').textContent = 'W1 · Tick #' + state.tick;
  el('totalSide').textContent = state.players.length + ' Spieler';
  el('peerFrames').textContent = format(state.peerHz, 0) + ' angewandte Peer-Updates/s';
  el('peerLinks').textContent = state.peerLinks;
  groupsPanel(state.players);
  workersPanel(state.workers);
  inspector(state.players);
  eventsPanel(state.events);
  map.options.player = chosen;
  map.update(state);
  chart();
}
el('color').addEventListener('change', () => {
  map.options.color = el('color').value;
  setLegend();
  map.schedule();
});
for (const key of ['grid', 'hulls', 'links', 'trails'])
  el(key).addEventListener('change', () => {
    map.options[key] = el(key).checked;
    map.schedule();
  });
el('zoomIn').onclick = () => map.zoom(1.3);
el('zoomOut').onclick = () => map.zoom(1 / 1.3);
el('fit').onclick = () => map.fit();
el('player').onchange = () => {
  chosen = Number(el('player').value) || null;
  selectedCluster = null;
  if (shown) render(shown);
};
el('freeze').onclick = () => {
  paused = !paused;
  const message = paused ? 'Ansicht fortsetzen' : 'Ansicht anhalten';
  el('freeze').title = message;
  el('freeze').setAttribute('aria-label', message);
  el('freeze').setAttribute('aria-pressed', paused);
  el('freeze').textContent = paused ? '▶' : 'Ⅱ';
  if (!paused && latest) render(latest);
  status(paused ? 'Ansicht pausiert' : 'Live', paused ? '' : 'live');
};
el('end').onclick = async () => {
  el('end').disabled = true;
  try {
    const response = await fetch('/api/stop', {
      method: 'POST',
      headers: { 'X-Mesh-Observer': '1' },
      signal: AbortSignal.timeout(2500),
    });
    if (!response.ok) throw Error('stop');
    ended = true;
    feed.close();
    clearTimeout(staleTimer);
    el('freeze').disabled = true;
    status('Beendet', 'stopped');
    el('end').textContent = 'Test beendet';
  } catch {
    el('end').disabled = false;
    status('Stoppen fehlgeschlagen', '');
  }
};
new ResizeObserver(chart).observe(el('chart'));
const feed = new EventSource('/api/live');
function show(frame) {
  if (ended || frame.status !== 'live') return;
  latest = frame;
  if (!paused) render(frame);
  status(paused ? 'Ansicht pausiert' : 'Live', paused ? '' : 'live');
  clearTimeout(staleTimer);
  staleTimer = setTimeout(() => status('Daten veraltet', ''), 2400);
}
feed.addEventListener('snapshot', (event) => show(JSON.parse(event.data)));
feed.addEventListener('sample', (event) => {
  const { point, ...frame } = JSON.parse(event.data);
  if (!latest) return;
  frame.history = latest.history.filter((p) => p.t >= frame.seconds - 60);
  if (point) frame.history.push(point);
  frame.events ??= latest.events;
  show(frame);
});
feed.onerror = () => {
  clearTimeout(staleTimer);
  if (!ended) status('Verbindung unterbrochen', '');
};
