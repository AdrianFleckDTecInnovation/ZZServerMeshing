export const clusterColors = ['#ad9aff', '#58d6b8', '#efa565', '#75b6fa', '#e38fc3', '#e1cf76'];
export const workerColors = ['#81cafa', '#b69afb', '#e7ac76', '#68d0b0'];
export const clusterColor = (id) => clusterColors[(id - 1) % clusterColors.length];
export const workerColor = (id) => workerColors[(id - 1) % workerColors.length];
export const clusterName = (id) => 'C' + String(id).padStart(2, '0');
export const playerName = (id) => 'P' + String(id).padStart(3, '0');
export const rateColor = (hz) =>
  hz === null ? '#8b98ac' : hz >= 17 ? '#64d9bc' : hz >= 8 ? '#ecb467' : '#f0848d';

function hull(points) {
  const sorted = points.sort((a, b) => a[0] - b[0] || a[1] - b[1]);
  function half(order) {
    const result = [];
    for (const p of order) {
      while (result.length >= 2) {
        const a = result.at(-2),
          b = result.at(-1);
        if ((b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0]) > 0) break;
        result.pop();
      }
      result.push(p);
    }
    result.pop();
    return result;
  }
  return [...half(sorted), ...half(sorted.reverse())];
}

export function context(canvas) {
  const w = canvas.clientWidth,
    h = canvas.clientHeight;
  const ratio = Math.min(2, devicePixelRatio || 1);
  if (canvas.width !== Math.round(w * ratio) || canvas.height !== Math.round(h * ratio)) {
    canvas.width = Math.round(w * ratio);
    canvas.height = Math.round(h * ratio);
  }
  const c = canvas.getContext('2d');
  c.setTransform(ratio, 0, 0, ratio, 0, 0);
  c.clearRect(0, 0, w, h);
  return { c, w, h };
}

function stepFor(metres) {
  const power = 10 ** Math.floor(Math.log10(metres));
  return ([1, 2, 5, 10].find((v) => v * power >= metres) ?? 10) * power;
}

export class UniverseMap {
  frame = null;
  trace = new Map();
  view = { x: 0, z: 0, scale: 1 };
  options = {
    grid: true,
    hulls: true,
    links: true,
    trails: false,
    color: 'worker',
    cluster: null,
    player: null,
  };
  fitted = false;
  pending = false;

  constructor(canvas, choose) {
    this.canvas = canvas;
    this.choose = choose;
    let down;
    const point = (e) => {
      const rect = canvas.getBoundingClientRect();
      return [e.clientX - rect.left, e.clientY - rect.top];
    };
    canvas.addEventListener(
      'wheel',
      (e) => {
        e.preventDefault();
        this.zoom(e.deltaY < 0 ? 1.2 : 1 / 1.2, ...point(e));
      },
      { passive: false },
    );
    canvas.addEventListener('pointerdown', (e) => {
      if (!e.isPrimary || e.button !== 0) return;
      canvas.setPointerCapture(e.pointerId);
      const [x, y] = point(e);
      down = { x, y, startX: this.view.x, startZ: this.view.z };
    });
    canvas.addEventListener('pointermove', (e) => {
      if (!down || !e.isPrimary) return;
      const [x, y] = point(e);
      this.view.x = down.startX - (x - down.x) / this.view.scale;
      this.view.z = down.startZ + (y - down.y) / this.view.scale;
      this.schedule();
    });
    canvas.addEventListener('pointerup', (e) => {
      if (!down || !e.isPrimary) return;
      const [x, y] = point(e);
      if (Math.hypot(x - down.x, y - down.y) < 5 && this.frame) {
        let best = null,
          nearest = 12;
        for (const p of this.frame.players) {
          const [px, py] = this.pixel(p.pos[0], p.pos[2]);
          const distance = Math.hypot(x - px, y - py);
          if (distance < nearest) {
            nearest = distance;
            best = p.id;
          }
        }
        this.choose(best);
      }
      down = null;
    });
    canvas.addEventListener('pointercancel', () => {
      down = null;
    });
    canvas.addEventListener('keydown', (e) => {
      const arrows = { ArrowLeft: [-1, 0], ArrowRight: [1, 0], ArrowUp: [0, 1], ArrowDown: [0, -1] };
      if (arrows[e.key]) {
        e.preventDefault();
        this.view.x += (arrows[e.key][0] * 32) / this.view.scale;
        this.view.z += (arrows[e.key][1] * 32) / this.view.scale;
        this.schedule();
      } else if (e.key === '+' || e.key === '=') this.zoom(1.25);
      else if (e.key === '-') this.zoom(0.8);
      else if (e.key === 'Home') {
        e.preventDefault();
        this.fit();
      }
    });
    new ResizeObserver(() => {
      if (this.fitted) this.fit();
      else this.schedule();
    }).observe(canvas);
  }
  update(frame) {
    if (frame.tick !== this.frame?.tick) {
      for (const p of frame.players) {
        const path = this.trace.get(p.id) ?? [];
        path.push([p.pos[0], p.pos[2]]);
        if (path.length > 28) path.shift();
        this.trace.set(p.id, path);
      }
    }
    this.frame = frame;
    if (!this.fitted) this.fit();
    this.schedule();
  }
  fit() {
    if (!this.frame?.players.length) return;
    const pts = this.frame.players;
    const lowX = Math.min(...pts.map((p) => p.pos[0] - p.half));
    const highX = Math.max(...pts.map((p) => p.pos[0] + p.half));
    const lowZ = Math.min(...pts.map((p) => p.pos[2] - p.half));
    const highZ = Math.max(...pts.map((p) => p.pos[2] + p.half));
    this.view = {
      x: (lowX + highX) / 2,
      z: (lowZ + highZ) / 2,
      scale: Math.max(
        0.4,
        Math.min(
          (this.canvas.clientWidth - 50) / (highX - lowX + 6),
          (this.canvas.clientHeight - 58) / (highZ - lowZ + 5),
        ),
      ),
    };
    this.fitted = true;
    this.schedule();
  }
  zoom(factor, x = this.canvas.clientWidth / 2, y = this.canvas.clientHeight / 2) {
    const v = this.view,
      old = v.scale;
    v.scale = Math.max(0.5, Math.min(100, old * factor));
    v.x += (x - this.canvas.clientWidth / 2) * (1 / old - 1 / v.scale);
    v.z -= (y - this.canvas.clientHeight / 2) * (1 / old - 1 / v.scale);
    this.schedule();
  }
  pixel(x, z) {
    return [
      this.canvas.clientWidth / 2 + (x - this.view.x) * this.view.scale,
      this.canvas.clientHeight / 2 - (z - this.view.z) * this.view.scale,
    ];
  }
  schedule() {
    if (this.pending) return;
    this.pending = true;
    requestAnimationFrame(() => {
      this.pending = false;
      this.draw();
    });
  }
  drawGrid(c, w, h) {
    const { x, z, scale } = this.view;
    const major = stepFor(45 / scale),
      minor = major / 5;
    const left = x - w / (2 * scale),
      right = x + w / (2 * scale);
    const bottom = z - h / (2 * scale),
      top = z + h / (2 * scale);
    c.font = '9px Consolas, monospace';
    for (const spacing of [minor, major]) {
      c.strokeStyle = spacing === major ? '#243144' : '#162131';
      c.fillStyle = '#697b92';
      c.lineWidth = 1;
      c.beginPath();
      for (let i = Math.ceil(left / spacing); i <= Math.floor(right / spacing); i++) {
        const [px] = this.pixel(i * spacing, 0);
        c.moveTo(px, 0);
        c.lineTo(px, h);
        if (spacing === major && px > 40 && px < w - 55)
          c.fillText(String(+(i * spacing).toFixed(2)), px + 3, h - 5);
      }
      for (let i = Math.ceil(bottom / spacing); i <= Math.floor(top / spacing); i++) {
        const [, py] = this.pixel(0, i * spacing);
        c.moveTo(0, py);
        c.lineTo(w, py);
        if (spacing === major && py > 36 && py < h - 33)
          c.fillText(String(+(i * spacing).toFixed(2)), 4, py - 3);
      }
      c.stroke();
    }
    const [ox, oy] = this.pixel(0, 0);
    c.strokeStyle = '#435678';
    c.setLineDash([3, 5]);
    c.beginPath();
    c.moveTo(ox, 0);
    c.lineTo(ox, h);
    c.moveTo(0, oy);
    c.lineTo(w, oy);
    c.stroke();
    c.setLineDash([]);
  }
  draw() {
    const { c, w, h } = context(this.canvas);
    if (!w || !h) return;
    const sky = c.createRadialGradient(w * 0.48, h * 0.48, 4, w * 0.5, h * 0.5, Math.max(w, h) * 0.68);
    sky.addColorStop(0, '#152135');
    sky.addColorStop(1, '#0a101a');
    c.fillStyle = sky;
    c.fillRect(0, 0, w, h);
    if (this.options.grid) this.drawGrid(c, w, h);
    if (!this.frame) return;
    const selected = this.frame.players.find((p) => p.id === this.options.player);
    if (this.options.links && selected) {
      const neighbors = new Set(selected.neighbors);
      const [x, y] = this.pixel(selected.pos[0], selected.pos[2]);
      c.beginPath();
      for (const p of this.frame.players)
        if (neighbors.has(p.id)) {
          c.moveTo(x, y);
          c.lineTo(...this.pixel(p.pos[0], p.pos[2]));
        }
      c.strokeStyle = clusterColor(selected.cluster) + '80';
      c.lineWidth = 0.9;
      c.stroke();
    }
    const groups = Map.groupBy(this.frame.players, (p) => p.cluster);
    for (const [id, players] of groups) {
      const color = clusterColor(id);
      const focused = this.options.cluster === null || this.options.cluster === id;
      c.globalAlpha = focused ? 1 : 0.27;
      const corners = [];
      for (const p of players)
        for (const dx of [-p.half, p.half])
          for (const dz of [-p.half, p.half]) corners.push(this.pixel(p.pos[0] + dx, p.pos[2] + dz));
      const outline = hull(corners);
      if (this.options.hulls && outline.length) {
        c.beginPath();
        outline.forEach(([x, y], i) => (i ? c.lineTo(x, y) : c.moveTo(x, y)));
        c.closePath();
        c.fillStyle = color + '10';
        c.fill();
        c.strokeStyle = color + '85';
        c.lineWidth = 1;
        c.setLineDash([4, 3]);
        c.stroke();
        c.setLineDash([]);
        const boundsX = Math.min(...outline.map((p) => p[0])),
          boundsY = Math.min(...outline.map((p) => p[1]));
        const owners = [...new Set(players.map((p) => p.worker))].sort();
        const label = clusterName(id) + ' · ' + owners.map((w) => 'W' + w).join('+');
        c.font = '10px Consolas, monospace';
        const width = c.measureText(label).width + 10;
        if (boundsX + width > 0 && boundsX < w && boundsY > 4 && boundsY < h + 10) {
          c.fillStyle = '#101725ed';
          c.fillRect(boundsX + 3, boundsY - 8, width, 15);
          c.strokeStyle = color + '83';
          c.strokeRect(boundsX + 3, boundsY - 8, width, 15);
          c.fillStyle = color;
          c.fillText(label, boundsX + 8, boundsY + 3);
        }
      }
      for (const p of players) {
        const [x, y] = this.pixel(p.pos[0], p.pos[2]);
        const dot =
          this.options.color === 'worker'
            ? workerColor(p.worker)
            : this.options.color === 'hz'
              ? rateColor(p.hz)
              : color;
        if (this.options.trails) {
          c.beginPath();
          (this.trace.get(p.id) ?? []).forEach(([px, pz], i) => {
            const screen = this.pixel(px, pz);
            if (i) c.lineTo(...screen);
            else c.moveTo(...screen);
          });
          c.strokeStyle = dot + '80';
          c.lineWidth = 1.4;
          c.stroke();
        }
        c.fillStyle = dot + '26';
        c.beginPath();
        c.arc(x, y, 5.4, 0, Math.PI * 2);
        c.fill();
        c.fillStyle = dot;
        c.beginPath();
        c.arc(x, y, 2.8, 0, Math.PI * 2);
        c.fill();
        if (this.options.player === p.id) {
          c.strokeStyle = '#f2f5fd';
          c.lineWidth = 1.3;
          c.beginPath();
          c.arc(x, y, 7, 0, Math.PI * 2);
          c.stroke();
        }
      }
    }
    c.globalAlpha = 1;
    const metres = stepFor(30 / this.view.scale),
      length = metres * this.view.scale;
    c.fillStyle = '#101826e8';
    c.fillRect(8, h - 30, length + 13, 22);
    c.strokeStyle = '#a1adc1';
    c.lineWidth = 1;
    c.beginPath();
    c.moveTo(14, h - 16);
    c.lineTo(14, h - 13);
    c.lineTo(14 + length, h - 13);
    c.lineTo(14 + length, h - 16);
    c.stroke();
    c.fillStyle = '#c8d1df';
    c.font = '9px Consolas, monospace';
    c.fillText(metres + ' m', 15, h - 18);
  }
}
