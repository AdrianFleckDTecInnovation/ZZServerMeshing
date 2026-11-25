import { execFileSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';

// The original repository omitted Cargo.lock and its service entry points only compiled on Unix.
export const sourceRevision = '3ef0d789196aa6c16de4fa12be394bdf7f8ea835';
const portableRevision = 'a2c478bf8f6e048fd370d9fda85e96aeeba133aa';

export function buildReference() {
  const target = resolve('target');
  mkdirSync(target, { recursive: true });
  const source = mkdtempSync(join(target, 'benchmark-reference-'));
  try {
    const archive = join(source, 'reference.tar');
    execFileSync('git', ['archive', '--format=tar', '--output', archive, sourceRevision]);
    execFileSync('tar', ['-xf', archive, '-C', source]);
    const files = ['Cargo.lock'];
    if (process.platform === 'win32') files.push('crates/coordinator/src/main.rs', 'crates/sim-server/src/main.rs');
    for (const file of files) {
      writeFileSync(join(source, file), execFileSync('git', ['show', `${portableRevision}:${file}`]));
    }
    const destination = join(target, 'benchmark-original-build');
    execFileSync('cargo', ['build', '--release', '--locked', '--manifest-path', join(source, 'Cargo.toml'),
      '-p', 'sim-server', '-p', 'coordinator', '--target-dir', destination], { stdio: 'inherit' });
    return join(destination, 'release');
  } finally {
    rmSync(source, { recursive: true, force: true });
  }
}
