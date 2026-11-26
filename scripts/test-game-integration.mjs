import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

// Build the documented example as a separate Cargo project against the committed checkout.
const repo = fileURLToPath(new URL('../', import.meta.url));
const git = (...args) => execFileSync('git', args, { cwd: repo, encoding: 'utf8' });
const revision = git('rev-parse', 'HEAD').trim();
const modified = git('status', '--porcelain', '--', 'crates/zz-mesh').trim().length > 0;
const library = modified
  ? `{ path = ${JSON.stringify(resolve(repo, 'crates/zz-mesh').replaceAll('\\', '/'))} }`
  : `{ git = "${pathToFileURL(repo).href}", rev = "${revision}" }`;
const example = name => readFileSync(join(repo, 'crates/zz-mesh/examples', name + '.rs'), 'utf8');
const game = mkdtempSync(join(tmpdir(), 'zz-mesh-game-'));
try {
  mkdirSync(join(game, 'src/bin'), { recursive: true });
  writeFileSync(join(game, 'src', 'main.rs'), example('persistent_game'));
  writeFileSync(join(game, 'src/bin/session_game.rs'), example('session_game'));
  writeFileSync(join(game, 'Cargo.toml'), `[package]
name = "mesh-game-smoke"
version = "0.1.0"
edition = "2021"

[dependencies]
zz-mesh = ${library}
bevy_app = "0.15"
bevy_ecs = "0.15"
serde = { version = "1", features = ["derive"] }
`);
  const env = { ...process.env, CARGO_TARGET_DIR: resolve(process.env.CARGO_TARGET_DIR || join(repo, 'target')) };
  for (const energy of [1, 2]) {
    const args = energy === 1 ? ['run', '--quiet'] : ['run', '--quiet', '--locked'];
    const result = execFileSync('cargo', [...args, '--bin', 'mesh-game-smoke', '--', 'world.sqlite'], {
      cwd: game, env, encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'],
    }).trim();
    assert.equal(result, `reactor=00000000-0000-0000-0000-000000000001 energy=${energy}`);
    console.log(result);
  }
  execFileSync('cargo', ['run', '--quiet', '--locked', '--bin', 'session_game'], { cwd: game, env, stdio: 'inherit' });
  console.log(`Game integration passed at ${modified ? 'working tree based on ' : ''}${revision}`);
} finally {
  rmSync(game, { recursive: true, force: true });
}
