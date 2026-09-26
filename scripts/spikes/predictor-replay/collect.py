#!/usr/bin/env python3
"""Replay past integrated runs: build the plan-review (a) and claim (b) inputs and the outcomes.

Reads the production queue only through read-only commands of the pinned binary
(`~/.local/bin/dagq stats --full --since 0`, `events --all --full`, `show --full`) and Git.
Writes out/dataset.json. Every input is cut at an event id (the submit or the claim) and at
a main commit that does not contain the run's own squash commit; `check_no_leak` asserts it.

Usage: python3 collect.py [--repo PATH] [--out DIR] [--sample N] [--seed S]
"""

import argparse
import collections
import json
import math
import os
from pathlib import Path
import random
import re
import subprocess
import sys

HERE = Path(__file__).resolve().parent
DAGQ = os.path.expanduser('~/.local/bin/dagq')
PROJECTS = Path(os.path.expanduser('~/.claude/projects'))


def sh(*args, cwd):
    return subprocess.run(args, cwd=cwd, check=True, capture_output=True, text=True).stdout


def dagq_json(repo, *args):
    return json.loads(sh(DAGQ, *args, cwd=repo))


def cached(path, make):
    if path.exists():
        return json.loads(path.read_text())
    value = make()
    path.write_text(json.dumps(value, ensure_ascii=False))
    return value


# ---------- outcomes ----------

def transcript_path(queue_hash, run_id):
    name = f'-Users-shinnosukeooyama--local-share-dagq-{queue_hash}-runs-{run_id}-worktree'
    return PROJECTS / name / f'{run_id}.jsonl'


def parse_ts(ts):
    from datetime import datetime
    return datetime.fromisoformat(ts.replace('Z', '+00:00')).timestamp()


def transcript_outcome(path):
    """Output tokens and model seconds of the worker's main thread (isSidechain excluded).

    One assistant message is written as several lines with the same message id; the usage is
    counted once per id (max). Model seconds: for each assistant message, from the preceding
    non-assistant line of the main thread to the message's last line.
    """
    entries = []
    for line in open(path):
        o = json.loads(line)
        if o.get('type') not in ('user', 'assistant') or o.get('isSidechain'):
            continue
        if 'timestamp' not in o:
            continue
        entries.append(o)
    tokens = {}
    per_line = 0
    turns = []  # [message id, start ts, end ts]
    prev_ts = None
    current = None
    for o in entries:
        ts = parse_ts(o['timestamp'])
        if o['type'] == 'assistant':
            mid = o['message'].get('id')
            out = (o['message'].get('usage') or {}).get('output_tokens', 0)
            tokens[mid] = max(tokens.get(mid, 0), out)
            per_line += out
            if current and current[0] == mid:
                current[2] = ts
            else:
                start = prev_ts if prev_ts is not None else ts
                current = [mid, start, ts]
                turns.append(current)
        else:
            current = None
            prev_ts = ts
            continue
        prev_ts = ts
    model_secs = sum(max(0.0, end - start) for _, start, end in turns)
    return {'output_tokens': sum(tokens.values()), 'output_tokens_per_line_sum': per_line, 'model_secs': round(model_secs, 1),
            'assistant_messages': len(tokens)}


def squash_commit(repo, run_id):
    out = sh('git', 'log', 'main', '--format=%H', f'--grep=^Dagq-Run: {run_id}$', cwd=repo).split()
    return out[0] if out else None


def changed_lines(repo, commit):
    added = deleted = files = 0
    for line in sh('git', 'show', '--numstat', '--format=', commit, cwd=repo).splitlines():
        a, d, _ = line.split('\t', 2)
        files += 1
        if a != '-':
            added += int(a)
            deleted += int(d)
    return {'added': added, 'deleted': deleted, 'lines': added + deleted, 'files': files}


# ---------- reconstructing the past ----------

BODY_FIELDS = ('title', 'description', 'acceptance', 'context', 'paths',
               'verification_commands', 'required_evidence', 'kind')


def body_at(task, events, cutoff):
    """The task body as it was right after event `cutoff`: undo later task_edited events."""
    body = {k: task.get(k) for k in BODY_FIELDS}
    later = [e for e in events if e['kind'] == 'task_edited' and e['id'] > cutoff]
    for e in sorted(later, key=lambda e: -e['id']):
        body.update(e['payload'].get('from', {}))
    return body


def deps_at(current, events, cutoff):
    deps = set(current)
    for e in sorted((e for e in events if e['id'] > cutoff), key=lambda e: -e['id']):
        pred = (e.get('payload') or {}).get('predecessor_id')
        if e['kind'] == 'dependency_added':
            deps.discard(pred)
        elif e['kind'] == 'dependency_removed':
            deps.add(pred)
    return sorted(deps)


def main_at(repo, when):
    return sh('git', 'rev-list', '-1', '--first-parent', f'--before={when}', 'main', cwd=repo).strip()


PATH_RE = re.compile(r'(?<![\w/.-])((?:src|tests|docs|plugins|scripts|migrations|\.github)/[\w./*-]+|'
                     r'(?:AGENTS|README|CLAUDE)\.md|Cargo\.toml|dagq\.toml|build\.rs)')


def main_view(repo, commit, body):
    """What a predictor can read from main at `commit`: sizes of the files the body names."""
    text = ' '.join(str(body.get(k) or '') for k in ('title', 'description', 'acceptance', 'context'))
    named = sorted({m.rstrip('.,、。)）') for m in PATH_RE.findall(text)})
    files = sh('git', 'ls-tree', '-r', '--name-only', commit, cwd=repo).splitlines()
    fileset = set(files)
    sizes = {}
    for p in named[:25]:
        if p in fileset:
            sizes[p] = sh('git', 'show', f'{commit}:{p}', cwd=repo).count('\n')
        else:
            under = [f for f in files if f.startswith(p.rstrip('/') + '/')]
            sizes[p] = f'dir, {len(under)} files' if under else 'absent'
    adrs = [int(m.group(1)) for f in files if (m := re.match(r'docs/adr/(\d{4})-', f))]
    migs = [int(m.group(1)) for f in files if (m := re.match(r'migrations/(\d{4})_', f))]
    src_rs = [f for f in files if f.startswith('src/') and f.endswith('.rs')]
    test_rs = [f for f in files if f.startswith('tests/') and f.endswith('.rs')]
    return {'commit': commit, 'named_file_lines': sizes, 'latest_adr': max(adrs, default=0),
            'latest_migration': max(migs, default=0), 'src_rs_files': len(src_rs),
            'tests_rs_files': len(test_rs)}


def check_no_leak(repo, item, run_events_min_id):
    """Assert the inputs hold nothing from the run itself or after the cut."""
    for key in ('a', 'b'):
        inp = item[key]
        # the claim event is the run's first event; nothing of the run comes after the cut
        assert inp['cutoff_event_id'] <= run_events_min_id, (item['run_id'], key, 'cutoff after the run began')
        main = inp['main']['commit']
        r = subprocess.run(['git', 'merge-base', '--is-ancestor', item['squash_commit'], main], cwd=repo)
        assert r.returncode == 1, (item['run_id'], key, 'main already holds the squash commit')
        for pred in inp.get('predecessors', []):
            assert pred['landed_event_id'] < inp['cutoff_event_id'], (item['run_id'], 'predecessor landed later')
        blob = json.dumps(inp, ensure_ascii=False)
        assert item['run_id'] not in blob, (item['run_id'], key, 'run id in input')
        assert item['squash_commit'] not in blob, (item['run_id'], key, 'squash commit in input')


# ---------- sampling ----------

def category(title, kind):
    prefix = re.split(r'[:：]', title, 1)[0].strip().lower() if re.search(r'[:：]', title) else ''
    if prefix.startswith('runtime') or kind == 'runtime':
        return 'runtime'
    if prefix in ('application', 'domain', 'supervisor', 'stats', 'list', 'show', 'fix'):
        return 'runtime'
    if prefix.startswith('docs'):
        return 'docs'
    if prefix.startswith('test') or prefix in ('ci', 'build', 'e2e', 'config'):
        return 'test/build'
    if prefix.startswith('plugin') or 'skill' in prefix:
        return 'plugin'
    return 'other'


def choose_sample(pool, n, seed):
    """Stratify by category x tercile of changed lines, allocate proportionally (at least 2 per
    non-empty cell), draw within a cell at random with a fixed seed."""
    lines = sorted(p['outcome']['changed']['lines'] for p in pool)
    t1, t2 = lines[len(lines) // 3], lines[2 * len(lines) // 3]
    cells = collections.defaultdict(list)
    for p in pool:
        size = p['outcome']['changed']['lines']
        tercile = 0 if size <= t1 else 1 if size <= t2 else 2
        p['stratum'] = f"{p['category']}/{tercile}"
        cells[p['stratum']].append(p)
    rng = random.Random(seed)
    quota = {k: max(min(2, len(v)), round(n * len(v) / len(pool))) for k, v in cells.items()}
    chosen = []
    for k in sorted(cells):
        v = sorted(cells[k], key=lambda p: p['run_id'])
        chosen += rng.sample(v, min(quota[k], len(v)))
    return chosen, {'tercile_bounds': [t1, t2], 'cells': {k: [len(cells[k]), quota[k]] for k in sorted(cells)}}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--repo', default=str(HERE.parents[2]))
    ap.add_argument('--out', default=str(HERE / 'out'))
    ap.add_argument('--sample', type=int, default=60)
    ap.add_argument('--seed', type=int, default=553)
    ap.add_argument('--queue-hash', default='77067154921b9014', help='the queue directory name under ~/.local/share/dagq')
    ap.add_argument('--until', default='2026-09-26T10:30:00Z', help='only runs finished before this (fixes the pool)')
    args = ap.parse_args()
    repo, out = args.repo, Path(args.out)
    (out / 'show').mkdir(parents=True, exist_ok=True)

    stats = cached(out / 'stats.json', lambda: dagq_json(repo, 'stats', '--full', '--since', '0', '--until', args.until))
    events = cached(out / 'events.json', lambda: dagq_json(repo, 'events', '--all', '--full', '--limit', '1000000'))['events']
    queue_hash = args.queue_hash

    by_run = collections.defaultdict(list)
    by_task = collections.defaultdict(list)
    for e in events:
        if e.get('run_id'):
            by_run[e['run_id']].append(e)
        if e.get('task_id') is not None:
            by_task[e['task_id']].append(e)

    pool = []
    for r in stats['runs']:
        if r['status'] != 'integrated':
            continue
        tpath = transcript_path(queue_hash, r['run_id'])
        if not tpath.exists():
            continue
        squash = squash_commit(repo, r['run_id'])
        if not squash:
            continue
        integ_fail = sum(1 for e in by_run[r['run_id']] if e['kind'] == 'verification_command'
                         and 'integrate-' in (e['payload'].get('log_path') or '') and e['payload'].get('exit_code') != 0)
        verdict = r.get('review_verdict')
        outcome = {
            **transcript_outcome(tpath),
            'stats_model_secs': (r.get('work_breakdown') or {}).get('secs', {}).get('model'),
            'work_secs': r.get('work'),
            'changed': changed_lines(repo, squash),
            'resumes': r.get('resumes', 0),
            'review_verdict': verdict,
            'integrate_verify_failures': integ_fail,
        }
        outcome['rework'] = int(outcome['resumes'] > 0 or verdict in ('revise', 'concern'))
        pool.append({'run_id': r['run_id'], 'task_id': r['task_id'], 'title': r['title'],
                     'category': category(r['title'], r.get('kind')), 'squash_commit': squash,
                     'outcome': outcome})
    sample, strata = choose_sample(pool, args.sample, args.seed)
    for p in sample:
        p['supplement'] = False
    # The stratified sample holds few reworked runs; add every other reworked run of the pool as a
    # supplement so the rework AUC has enough positives (the heaviness metrics use the sample only).
    chosen = {p['run_id'] for p in sample}
    for p in pool:
        if p['outcome']['rework'] and p['run_id'] not in chosen:
            p['supplement'] = True
            p.setdefault('stratum', p['category'] + '/supplement')
            sample.append(p)

    items = []
    for p in sorted(sample, key=lambda p: p['task_id']):
        rid, tid = p['run_id'], p['task_id']
        show = cached(out / 'show' / f'{tid}.json', lambda: dagq_json(repo, 'show', str(tid), '--full'))
        tevents = sorted(by_task[tid], key=lambda e: e['id'])
        run = next(x for x in show['runs'] if x['id'] == rid)
        claim = next(e for e in tevents if e['kind'] == 'run_claimed' and e['run_id'] == rid)
        run_min_id = min(e['id'] for e in by_run[rid])
        before = [e for e in tevents if e['id'] < claim['id']]
        submits = [e for e in before if e['kind'] == 'task_submitted']
        readies = [e for e in before if e['kind'] == 'task_status_changed' and e['payload'].get('to') == 'ready']
        sub = submits[-1] if submits else readies[-1]
        prev_runs = [x['status'] for x in show['runs'] if x['created_at'] < run['created_at']]

        def build(cut_event, main_commit, with_preds):
            body = body_at(show['task'], tevents, cut_event['id'])
            deps = deps_at(show['dependencies'], tevents, cut_event['id'])
            inp = {'cutoff_event_id': cut_event['id'], 'cutoff_at': cut_event['created_at'],
                   'task': {'id': tid, **body, 'dependencies': deps},
                   'main': main_view(repo, main_commit, body)}
            if with_preds:
                preds = []
                for d in deps:
                    landed = [e for e in by_task[d] if e['kind'] == 'run_integrated' and e['id'] < cut_event['id']]
                    if not landed:
                        continue
                    lr = landed[-1]['run_id']
                    recs = [e for e in by_run[lr] if e['kind'] == 'integration_receipt' and e['id'] < landed[-1]['id']]
                    summary = recs[-1]['payload']['receipt'].get('summary') if recs else None
                    preds.append({'task_id': d, 'landed_event_id': landed[-1]['id'], 'summary': summary})
                inp['predecessors'] = preds
                inp['earlier_runs_of_task'] = prev_runs
            return inp

        item = {k: p[k] for k in ('run_id', 'task_id', 'title', 'category', 'stratum', 'supplement',
                                  'squash_commit', 'outcome')}
        item['submit_kind'] = sub['kind']
        item['a'] = build(sub, main_at(repo, sub['created_at']), False)
        item['b'] = build(claim, run['base_commit'], True)
        check_no_leak(repo, item, run_min_id)
        items.append(item)
        print(f"task {tid} run {rid[:8]} {item['stratum']} tokens={item['outcome']['output_tokens']}", file=sys.stderr)

    (out / 'dataset.json').write_text(json.dumps(
        {'pool_size': len(pool), 'pool_categories': collections.Counter(p['category'] for p in pool),
         'strata': strata, 'seed': args.seed, 'until': args.until, 'items': items}, ensure_ascii=False, indent=1))
    print(f'pool {len(pool)}, sample {len(items)}', file=sys.stderr)


if __name__ == '__main__':
    main()
