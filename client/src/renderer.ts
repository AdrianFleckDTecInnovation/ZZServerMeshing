import * as THREE from 'three/webgpu';
import { OrbitControls } from 'three/addons/controls/OrbitControls.js';
import { WorldView, SERVER_COLORS, PLAYER_COLOR } from './world-view';
import { NetworkClient } from './network';

export class Renderer {
  private renderer!: THREE.WebGPURenderer;
  private scene!: THREE.Scene;
  private camera!: THREE.PerspectiveCamera;
  private controls!: OrbitControls;
  private worldView: WorldView;
  private network: NetworkClient;

  // Individual meshes per entity
  private entityMeshes: Map<string, THREE.Mesh> = new Map();

  // Shared geometries
  private npcGeometry!: THREE.SphereGeometry;
  private playerGeometry!: THREE.BoxGeometry;

  private raycaster = new THREE.Raycaster();
  private groundPlane = new THREE.Plane(new THREE.Vector3(0, 1, 0), 0);
  private mouse = new THREE.Vector2();

  private lastTime = performance.now();

  constructor(worldView: WorldView, network: NetworkClient) {
    this.worldView = worldView;
    this.network = network;
  }

  async init(): Promise<void> {
    // Scene
    this.scene = new THREE.Scene();
    this.scene.background = new THREE.Color(0x1a1a2e);

    // Camera: elevated perspective looking down
    this.camera = new THREE.PerspectiveCamera(
      60,
      window.innerWidth / window.innerHeight,
      0.1,
      2000
    );
    this.camera.position.set(0, 300, 300);
    this.camera.lookAt(0, 0, 0);

    // Renderer
    this.renderer = new THREE.WebGPURenderer({ antialias: true });
    this.renderer.setSize(window.innerWidth, window.innerHeight);
    this.renderer.setPixelRatio(window.devicePixelRatio);
    document.body.appendChild(this.renderer.domElement);
    await this.renderer.init();

    // Controls
    this.controls = new OrbitControls(this.camera, this.renderer.domElement);
    this.controls.enableDamping = true;
    this.controls.dampingFactor = 0.1;
    this.controls.maxPolarAngle = Math.PI / 2.2;
    this.controls.minDistance = 50;
    this.controls.maxDistance = 1000;

    // Lighting
    const ambient = new THREE.AmbientLight(0x606080, 2);
    this.scene.add(ambient);

    const directional = new THREE.DirectionalLight(0xffffff, 3);
    directional.position.set(200, 400, 200);
    this.scene.add(directional);

    // Ground plane
    const groundGeom = new THREE.PlaneGeometry(1000, 1000);
    const groundMat = new THREE.MeshStandardNodeMaterial();
    groundMat.color = new THREE.Color(0x1a2a1a);
    const ground = new THREE.Mesh(groundGeom, groundMat);
    ground.rotation.x = -Math.PI / 2;
    ground.position.y = -0.5;
    this.scene.add(ground);

    // Grid helper
    const grid = new THREE.GridHelper(1000, 20, 0x333333, 0x222222);
    this.scene.add(grid);

    // World bounds wireframe
    const boundsGeom = new THREE.BoxGeometry(1000, 10, 1000);
    const boundsEdges = new THREE.EdgesGeometry(boundsGeom);
    const boundsMat = new THREE.LineBasicMaterial({ color: 0x444444 });
    const boundsLine = new THREE.LineSegments(boundsEdges, boundsMat);
    boundsLine.position.y = 5;
    this.scene.add(boundsLine);

    // Shared geometries
    this.npcGeometry = new THREE.SphereGeometry(4, 16, 12);
    this.playerGeometry = new THREE.BoxGeometry(6, 10, 6);

    // Event listeners
    window.addEventListener('resize', this.onResize.bind(this));
    this.renderer.domElement.addEventListener('click', this.onClick.bind(this));

    // Start render loop
    this.renderer.setAnimationLoop(this.animate.bind(this));
  }

  private animate(): void {
    const now = performance.now();
    const dt = (now - this.lastTime) / 1000;
    this.lastTime = now;

    // Interpolate entity positions
    this.worldView.interpolate(dt);

    // Sync meshes with world view
    this.syncMeshes();

    this.controls.update();
    this.renderer.render(this.scene, this.camera);
  }

  private syncMeshes(): void {
    const activeIds = new Set<string>();

    for (const entity of this.worldView.entities.values()) {
      activeIds.add(entity.id);

      let mesh = this.entityMeshes.get(entity.id);
      if (!mesh) {
        // Create new mesh for this entity
        const color = this.worldView.getColorForEntity(entity);
        const geom = entity.entityType === 'Player' ? this.playerGeometry : this.npcGeometry;
        const mat = new THREE.MeshStandardNodeMaterial();
        mat.color = color.clone();
        mat.roughness = 0.5;
        mat.metalness = 0.3;
        mesh = new THREE.Mesh(geom, mat);
        this.scene.add(mesh);
        this.entityMeshes.set(entity.id, mesh);
      }

      // Update position
      const yOffset = entity.entityType === 'Player' ? 5 : 4;
      mesh.position.set(entity.position.x, entity.position.y + yOffset, entity.position.z);

      // Update color if authority server changed
      const targetColor = this.worldView.getColorForEntity(entity);
      const mat = mesh.material as THREE.MeshStandardNodeMaterial;
      if (!mat.color.equals(targetColor)) {
        mat.color.copy(targetColor);
      }
    }

    // Remove meshes for entities that no longer exist
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
      const entityType = event.shiftKey ? 'player' : 'npc';
      this.network.spawnEntity(intersection.x, 0, intersection.z, entityType);
    }
  }

  private onResize(): void {
    this.camera.aspect = window.innerWidth / window.innerHeight;
    this.camera.updateProjectionMatrix();
    this.renderer.setSize(window.innerWidth, window.innerHeight);
  }
}
