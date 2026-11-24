#!/usr/bin/env python3
"""Inspect incremental caches; --apply prunes them while holding Cargo locks."""
import argparse
import os
from pathlib import Path
import shutil
import time


def main():
    import fcntl
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--apply', action='store_true')
    parser.add_argument('--max-gib', type=float, default=6)
    parser.add_argument('--max-days', type=float, default=3)
    args = parser.parse_args()
    if not 0 <= args.max_gib < float('inf') or not 0 <= args.max_days < float('inf'):
        parser.error('limits must be finite nonnegative numbers')
    root = Path(__file__).resolve().parent.parent
    target = Path(os.environ.get('CARGO_TARGET_DIR', root / '.cache/target')).resolve()
    if not target.exists():
        return
    profiles = sorted({p.parent for pattern in ('*/.cargo-lock', '*/*/.cargo-lock')
                       for p in target.glob(pattern) if not p.parent.is_symlink()})
    locks = []
    idle_profiles = []
    busy_profiles = []
    try:
        for profile in profiles:
            lock = (profile / '.cargo-lock').open('r+')
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                lock.close()
                busy_profiles.append(profile)
                print('Build cache profile skipped: Cargo profile is busy:', profile)
                continue
            locks.append(lock)
            idle_profiles.append(profile)
        entries = []
        protected = 0
        for profile in idle_profiles:
            incremental = profile / 'incremental'
            if not incremental.is_dir() or incremental.is_symlink():
                continue
            for crate in incremental.iterdir():
                if not crate.is_dir() or crate.is_symlink():
                    continue
                size = sum(p.lstat().st_blocks * 512 for p in crate.rglob('*')
                           if not p.is_symlink())
                if any(p.name.endswith('-working') for p in crate.iterdir()):
                    protected += size
                    continue
                entries.append((crate.stat().st_mtime, size, crate))
        total = protected + sum(size for _, size, _ in entries)
        initial = total
        cutoff = time.time() - args.max_days * 86400
        for modified, size, crate in sorted(entries):
            if modified >= cutoff and total <= args.max_gib * 1024**3:
                continue
            print(('Remove' if args.apply else 'Would remove'), crate.relative_to(target),
                  f'{size / 1024**3:.2f} GiB')
            if args.apply:
                shutil.rmtree(crate)
            total -= size
        print(f'Incremental cache: {initial / 1024**3:.2f} -> {total / 1024**3:.2f} GiB'
              + ('' if args.apply else ' (preview)'))
        if busy_profiles:
            print('Busy profiles were excluded from this total; their caches were preserved.')
        if total > args.max_gib * 1024**3:
            print('Capacity target not met: unfinished working sessions were preserved.')
    finally:
        for lock in reversed(locks):
            lock.close()


if __name__ == '__main__':
    main()
