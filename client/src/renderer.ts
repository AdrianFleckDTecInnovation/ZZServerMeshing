import * as THREE from 'three/webgpu';
import { OrbitControls } from 'three/addons/controls/OrbitControls.js';
import { WorldView, SERVER_COLORS, PLAYER_COLOR } from './world-view';
import { NetworkClient } from './network';

const LOCAL_PLAYER_COLOR = new THREE.Color(0x00ff88); // Bright green for YOUR player

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

  private raycaster = new THREE.Raycaster();
  private groundPlane = new THREE.Plane(new THREE.Vector3(0, 1, 0), 0);
  private mouse = new THREE.Vector2();

  private lastTime = performance.now();

  // Keyboard state
  private keys = new Set<string>();
  /** Camera follows player when true. Press F to toggle. */
  followPlayer = true;
  private cameraOffset = new THREE.Vector3(0, 200, 200);

  constructor(worldView: WorldView, network: NetworkClient) {
    this.worldView = worldView;
    this.network = network;
  }

  async init(): Promise<void> {
    this.scene = new THREE.Scene();
    this.scene.background = new THREE.Color(0x1a1a2e);

    this.camera = new THREE.PerspectiveCamera(
      60, window.innerWidth / window.innerHeight, 0.1, 2000
    );
    this.camera.position.set(0, 200, 200);
    this.camera.lookAt(0, 0, 0);

    this.renderer = new THREE.WebGPURenderer({ antialias: true });
    this.renderer.setSize(window.innerWidth, window.innerHeight);
    this.renderer.setPixelRatio(window.devicePixelRatio);
    document.body.appendChild(this.renderer.domElement);
    await this.renderer.init();

    this.controls = new OrbitControls(this.camera, this.renderer.domElement);
    this.controls.enableDamping = true;
    this.controls.dampingFactor = 0.1;
    this.controls.maxPolarAngle = Math.PI / 2.2;
    this.controls.minDistance = 30;
    this.controls.maxDistance = 1000;

    // Lighting
    this.scene.add(new THREE.AmbientLight(0x606080, 2));
    const dir = new THREE.DirectionalLight(0xffffff, 3);
    dir.position.set(200, 400, 200);
    this.scene.add(dir);

    // Ground
    const groundGeom = new THREE.PlaneGeometry(1000, 1000);
    const groundMat = new THREE.MeshStandardNodeMaterial();
    groundMat.color = new THREE.Color(0x1a2a1a);
    const ground = new THREE.Mesh(groundGeom, groundMat);
    ground.rotation.x = -Math.PI / 2;
    ground.position.y = -0.5;
    this.scene.add(ground);

    this.scene.add(new THREE.GridHelper(1000, 20, 0x333333, 0x222222));

    // World bounds
    const boundsEdges = new THREE.EdgesGeometry(new THREE.BoxGeometry(1000, 10, 1000));
    const boundsLine = new THREE.LineSegments(boundsEdges, new THREE.LineBasicMaterial({ color: 0x444444 }));
    boundsLine.position.y = 5;
    this.scene.add(boundsLine);

    // Shared geometries
    this.npcGeometry = new THREE.SphereGeometry(4, 16, 12);
    this.playerGeometry = new THREE.BoxGeometry(6, 10, 6);

    // Events
    window.addEventListener('resize', this.onResize.bind(this));
    this.renderer.domElement.addEventListener('click', this.onClick.bind(this));
    window.addEventListener('keydown', (e) => {
      this.keys.add(e.key.toLowerCase());
      if (e.key.toLowerCase() === 'f') {
        this.followPlayer = !this.followPlayer;
      }
    });
    window.addEventListener('keyup', (e) => this.keys.delete(e.key.toLowerCase()));

    this.renderer.setAnimationLoop(this.animate.bind(this));
  }

  private animate(): void {
    const now = performance.now();
    const dt = (now - this.lastTime) / 1000;
    this.lastTime = now;

    // Send WASD input for our player
    this.handleKeyboardInput();

    // Interpolate
    this.worldView.interpolate(dt);

    // Sync meshes
    this.syncMeshes();

    // Camera follow
    if (this.followPlayer && this.worldView.localPlayerId) {
      const player = this.worldView.entities.get(this.worldView.localPlayerId);
      if (player) {
        const target = player.position.clone().add(this.cameraOffset);
        this.camera.position.lerp(target, 0.05);
        this.controls.target.lerp(player.position, 0.05);
      }
    }

    this.controls.update();
    this.renderer.render(this.scene, this.camera);
  }

  private handleKeyboardInput(): void {
    if (!this.network.playerId) return;

    let dx = 0, dz = 0;
    if (this.keys.has('w') || this.keys.has('arrowup')) dz = -1;
    if (this.keys.has('s') || this.keys.has('arrowdown')) dz = 1;
    if (this.keys.has('a') || this.keys.has('arrowleft')) dx = -1;
    if (this.keys.has('d') || this.keys.has('arrowright')) dx = 1;

    if (dx !== 0 || dz !== 0) {
      this.network.moveInput(this.network.playerId, dx, 0, dz);
    }
  }

  private syncMeshes(): void {
    const activeIds = new Set<string>();
    const localId = this.worldView.localPlayerId;

    for (const entity of this.worldView.entities.values()) {
      activeIds.add(entity.id);

      let mesh = this.entityMeshes.get(entity.id);
      if (!mesh) {
        const geom = entity.entityType === 'Player' ? this.playerGeometry : this.npcGeometry;
        const mat = new THREE.MeshStandardNodeMaterial();
        mat.roughness = 0.5;
        mat.metalness = 0.3;
        // Local player gets a distinct color
        if (entity.id === localId) {
          mat.color = LOCAL_PLAYER_COLOR.clone();
          mat.emissive = new THREE.Color(0x003311);
        } else {
          mat.color = this.worldView.getColorForEntity(entity).clone();
        }
        mesh = new THREE.Mesh(geom, mat);
        this.scene.add(mesh);
        this.entityMeshes.set(entity.id, mesh);
      }

      const yOffset = entity.entityType === 'Player' ? 5 : 4;
      mesh.position.set(entity.position.x, entity.position.y + yOffset, entity.position.z);

      // Update color (skip local player — always green)
      if (entity.id !== localId) {
        const targetColor = this.worldView.getColorForEntity(entity);
        const mat = mesh.material as THREE.MeshStandardNodeMaterial;
        if (!mat.color.equals(targetColor)) {
          mat.color.copy(targetColor);
        }
      }
    }

    for (const [id, mesh] of this.entityMeshes) {
      if (!activeIds.has(id)) {
        this.scene.remove(mesh);
        (mesh.material as THREE.MeshStandardNodeMaterial).dispose();
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
