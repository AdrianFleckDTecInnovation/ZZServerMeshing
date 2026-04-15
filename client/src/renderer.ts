import * as THREE from 'three/webgpu';
import { OrbitControls } from 'three/addons/controls/OrbitControls.js';
import { WorldView, SERVER_COLORS, PLAYER_COLOR, EntityData } from './world-view';
import { NetworkClient } from './network';

const LOCAL_PLAYER_COLOR = new THREE.Color(0x00ff88);

export class Renderer {
  private renderer!: THREE.WebGPURenderer;
  private scene!: THREE.Scene;
  private camera!: THREE.PerspectiveCamera;
  private controls!: OrbitControls;
  private worldView: WorldView;
  private network: NetworkClient;

  private entityMeshes: Map<string, THREE.Mesh> = new Map();
  private npcGeometry!: THREE.SphereGeometry;
  private playerGeometry!: THREE.BoxGeometry;

  // Shared materials — one per server color, plus player/local player
  private serverMaterials: Map<number, THREE.MeshStandardNodeMaterial> = new Map();
  private playerMaterial!: THREE.MeshStandardNodeMaterial;
  private localPlayerMaterial!: THREE.MeshStandardNodeMaterial;

  // Mesh pools for reuse
  private npcMeshPool: THREE.Mesh[] = [];
  private playerMeshPool: THREE.Mesh[] = [];

  // AoI radius circle
  private aoiCircle!: THREE.Mesh;

  private raycaster = new THREE.Raycaster();
  private groundPlane = new THREE.Plane(new THREE.Vector3(0, 1, 0), 0);
  private mouse = new THREE.Vector2();
  private lastTime = performance.now();

  private keys = new Set<string>();
  followPlayer = true;
  onRenderFrame: (() => void) | null = null;
  private cameraOffset = new THREE.Vector3(0, 600, 0);

  constructor(worldView: WorldView, network: NetworkClient) {
    this.worldView = worldView;
    this.network = network;
  }

  async init(): Promise<void> {
    this.scene = new THREE.Scene();
    this.scene.background = new THREE.Color(0x1a1a2e);

    this.camera = new THREE.PerspectiveCamera(
      90, window.innerWidth / window.innerHeight, 0.1, 20000
    );
    this.camera.position.set(0, 2000, 0);
    this.camera.up.set(0, 0, -1); // Z-forward as "up" so lookAt points straight down
    this.camera.lookAt(0, 0, 0);

    this.renderer = new THREE.WebGPURenderer({ antialias: true });
    this.renderer.setSize(window.innerWidth, window.innerHeight);
    this.renderer.setPixelRatio(window.devicePixelRatio);
    document.body.appendChild(this.renderer.domElement);
    await this.renderer.init();

    // OrbitControls only active in free camera mode (press F)
    this.controls = new OrbitControls(this.camera, this.renderer.domElement);
    this.controls.enabled = false;

    this.scene.add(new THREE.AmbientLight(0x606080, 2));
    const dir = new THREE.DirectionalLight(0xffffff, 3);
    dir.position.set(200, 400, 200);
    this.scene.add(dir);

    const groundGeom = new THREE.PlaneGeometry(10000, 10000);
    const groundMat = new THREE.MeshStandardNodeMaterial();
    groundMat.color = new THREE.Color(0x1a2a1a);
    const ground = new THREE.Mesh(groundGeom, groundMat);
    ground.rotation.x = -Math.PI / 2;
    ground.position.y = -0.5;
    this.scene.add(ground);

    this.scene.add(new THREE.GridHelper(10000, 20, 0x333333, 0x222222));

    const boundsEdges = new THREE.EdgesGeometry(new THREE.BoxGeometry(10000, 10, 10000));
    const boundsLine = new THREE.LineSegments(boundsEdges, new THREE.LineBasicMaterial({ color: 0x444444 }));
    boundsLine.position.y = 5;
    this.scene.add(boundsLine);

    // AoI radius ring (500 units) — rendered as a thin ring mesh since WebGPU doesn't support lines well
    const aoiRadius = 500;
    const aoiRingGeom = new THREE.RingGeometry(aoiRadius - 3, aoiRadius + 3, 64);
    const aoiRingMat = new THREE.MeshBasicNodeMaterial();
    aoiRingMat.color = new THREE.Color(0x00ff00);
    aoiRingMat.opacity = 0.4;
    aoiRingMat.transparent = true;
    aoiRingMat.side = THREE.DoubleSide;
    this.aoiCircle = new THREE.Mesh(aoiRingGeom, aoiRingMat);
    this.aoiCircle.rotation.x = -Math.PI / 2; // lay flat on ground
    this.aoiCircle.position.y = 0.5;
    this.scene.add(this.aoiCircle);

    // Shared geometries
    this.npcGeometry = new THREE.SphereGeometry(12, 16, 12);
    this.playerGeometry = new THREE.BoxGeometry(16, 16, 16);

    // Pre-create shared materials
    for (let i = 0; i < SERVER_COLORS.length; i++) {
      const mat = new THREE.MeshStandardNodeMaterial();
      mat.roughness = 0.5;
      mat.metalness = 0.3;
      mat.color = SERVER_COLORS[i].clone();
      this.serverMaterials.set(i + 1, mat);
    }

    this.playerMaterial = new THREE.MeshStandardNodeMaterial();
    this.playerMaterial.roughness = 0.5;
    this.playerMaterial.metalness = 0.3;
    this.playerMaterial.color = PLAYER_COLOR.clone();

    this.localPlayerMaterial = new THREE.MeshStandardNodeMaterial();
    this.localPlayerMaterial.roughness = 0.5;
    this.localPlayerMaterial.metalness = 0.3;
    this.localPlayerMaterial.color = LOCAL_PLAYER_COLOR.clone();
    this.localPlayerMaterial.emissive = new THREE.Color(0x003311);

    // Events
    window.addEventListener('resize', this.onResize.bind(this));
    this.renderer.domElement.addEventListener('click', this.onClick.bind(this));
    window.addEventListener('keydown', (e) => {
      this.keys.add(e.key.toLowerCase());
      if (e.key.toLowerCase() === 'f') this.followPlayer = !this.followPlayer;
    });
    window.addEventListener('keyup', (e) => this.keys.delete(e.key.toLowerCase()));

    this.renderer.setAnimationLoop(this.animate.bind(this));
  }

  private getMaterialForEntity(entity: EntityData): THREE.MeshStandardNodeMaterial {
    if (entity.id === this.worldView.localPlayerId) return this.localPlayerMaterial;
    if (entity.entityType === 'Player') return this.playerMaterial;
    return this.serverMaterials.get(entity.authorityServer) ?? this.serverMaterials.get(1)!;
  }

  private acquireMesh(entity: EntityData): THREE.Mesh {
    const isPlayer = entity.entityType === 'Player';
    const pool = isPlayer ? this.playerMeshPool : this.npcMeshPool;
    let mesh = pool.pop();
    if (mesh) {
      mesh.material = this.getMaterialForEntity(entity);
    } else {
      const geom = isPlayer ? this.playerGeometry : this.npcGeometry;
      mesh = new THREE.Mesh(geom, this.getMaterialForEntity(entity));
    }
    this.scene.add(mesh);
    return mesh;
  }

  private releaseMesh(mesh: THREE.Mesh): void {
    this.scene.remove(mesh);
    const isPlayer = mesh.geometry === this.playerGeometry;
    (isPlayer ? this.playerMeshPool : this.npcMeshPool).push(mesh);
  }

  private animate(): void {
    const now = performance.now();
    const dt = (now - this.lastTime) / 1000;
    this.lastTime = now;

    this.handleKeyboardInput(dt);
    this.worldView.interpolate(dt);
    this.syncMeshes();

    if (this.followPlayer && this.worldView.localPlayerId) {
      this.controls.enabled = false;
      const player = this.worldView.entities.get(this.worldView.localPlayerId);
      if (player) {
        this.camera.position.set(player.position.x, 2000, player.position.z);
        this.camera.up.set(0, 0, -1);
        this.camera.lookAt(player.position.x, 0, player.position.z);
        this.aoiCircle.position.set(player.position.x, 1, player.position.z);
      }
    } else {
      this.controls.enabled = true;
      this.controls.update();
    }
    this.renderer.render(this.scene, this.camera);
    this.onRenderFrame?.();
  }

  private handleKeyboardInput(dt: number): void {
    if (!this.network.playerId) return;
    let dx = 0, dz = 0;
    if (this.keys.has('w') || this.keys.has('arrowup')) dz = -1;
    if (this.keys.has('s') || this.keys.has('arrowdown')) dz = 1;
    if (this.keys.has('a') || this.keys.has('arrowleft')) dx = -1;
    if (this.keys.has('d') || this.keys.has('arrowright')) dx = 1;
    if (dx !== 0 || dz !== 0) {
      const player = this.worldView.entities.get(this.network.playerId);
      if (player) {
        const seq = this.network.prediction.pushInput([dx, 0, dz], player, dt);
        this.network.moveInput(this.network.playerId, dx, 0, dz, seq);
      }
    }
  }

  private syncMeshes(): void {
    const activeIds = new Set<string>();
    const localId = this.worldView.localPlayerId;

    for (const entity of this.worldView.entities.values()) {
      activeIds.add(entity.id);

      let mesh = this.entityMeshes.get(entity.id);
      if (!mesh) {
        mesh = this.acquireMesh(entity);
        this.entityMeshes.set(entity.id, mesh);
      }

      // Stack entities vertically by server ID so overlapping ones are all visible from top-down
      const yOffset = entity.entityType === 'Player' ? 20 : (entity.authorityServer * 8);
      mesh.position.set(entity.position.x, entity.position.y + yOffset, entity.position.z);

      // Swap material if authority changed (skip local player)
      if (entity.id !== localId) {
        const correctMat = this.getMaterialForEntity(entity);
        if (mesh.material !== correctMat) {
          mesh.material = correctMat;
        }
      }
    }

    for (const [id, mesh] of this.entityMeshes) {
      if (!activeIds.has(id)) {
        this.releaseMesh(mesh);
        this.entityMeshes.delete(id);
      }
    }
  }

  private onClick(event: MouseEvent): void {
    this.mouse.x = (event.clientX / window.innerWidth) * 2 - 1;
    this.mouse.y = -(event.clientY / window.innerHeight) * 2 + 1;
    this.raycaster.setFromCamera(this.mouse, this.camera);

    const intersection = new THREE.Vector3();
    if (this.raycaster.ray.intersectPlane(this.groundPlane, intersection)) {
      this.network.spawnEntity(intersection.x, 0, intersection.z, 'npc');
    }
  }

  private onResize(): void {
    this.camera.aspect = window.innerWidth / window.innerHeight;
    this.camera.updateProjectionMatrix();
    this.renderer.setSize(window.innerWidth, window.innerHeight);
  }
}
