#!/usr/bin/env python3
"""Predict each replayed run's weight and rework from its (a) plan-review or (b) claim input.

Candidates: `rules` (mechanical features, weights fixed before looking at outcomes) and the LLMs
called headless (`claude -p --model ID [--effort L] --strict-mcp-config --tools ""`) with the same
prompt. The LLM sees only the input built by collect.py (never the outcome). At most 2 calls run at
once. Results are cached per (candidate, run, input) under out/predictions/.

Usage: python3 predict.py CANDIDATE [--inputs a,b] [--only-sample]
  CANDIDATE: rules | haiku | sonnet | opus-low | opus-high
"""

import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import math
import os
from pathlib import Path
import re
import subprocess
import sys
import time

HERE = Path(__file__).resolve().parent
OUT = HERE / 'out'

MODELS = {
    'haiku': ('claude-haiku-4-5-20251001', None),
    'sonnet': ('claude-sonnet-5', None),
    'opus-low': ('claude-opus-5-5', 'low'),
    'opus-high': ('claude-opus-5-5', 'high'),
}

SYSTEM = 'You estimate software tasks before they start. Reply with one JSON object and nothing else.'

PROMPT = """dagq は Rust の開発タスクを 1 task 1 worker で流す runtime です。worker は Claude（Opus）の 1 session で、
割り当てられた Git worktree の中で task を実装し、fmt・clippy・変更に関係する test（runtime を変えたら e2e も）を流し、
commit して receipt を書きます。その後 headless の review（pass / revise / concern）と、integrate の検証
（main へ rebase した後の verification_commands。runtime では cargo llvm-cov の 80% の関門）があり、検証の失敗・rebase の衝突・
evidence の不足では run が resume されて worker が直します。

次の task を worker が実行する前に、この task の重さと手戻りを見積もってください。入力は、見積もりの時点で分かっている情報だけです
（task の本文、依存、その時点の main で本文が名指すファイルの行数、最新の ADR と migration の番号{extra}）。

目安: worker 1 run の出力 token（thinking を含む）は小さいもので 5 千、大きいもので 25 万程度、中央値は 3.5 万程度です。

次のキーだけを持つ JSON を 1 つ返してください。
- "size": "S" | "M" | "L"
- "nature": "mechanical" | "implementation" | "design_judgment" | "investigation"
- "uncertainty": 0 から 1（1 がもっとも不確か）
- "expected_output_tokens": 整数（worker 1 run の出力 token の見込み）
- "rework_probability": 0 から 1（resume されるか review が revise / concern を出す確率）
- "reason": 1 文

入力:
{input}
"""

EXTRA_B = '、依存元の task の着地時の receipt の要約、この task の以前の run の結果'


def prompt_for(inp, which):
    shown = {k: v for k, v in inp.items() if k not in ('cutoff_event_id', 'cutoff_at')}
    return PROMPT.format(extra=EXTRA_B if which == 'b' else '', input=json.dumps(shown, ensure_ascii=False, indent=1))


# ---------- rules ----------

def features(inp):
    t = inp['task']
    title = t.get('title') or ''
    prefix = re.split(r'[:：]', title, 1)[0].strip().lower() if re.search(r'[:：]', title) else ''
    text = ' '.join(str(t.get(k) or '') for k in ('description', 'acceptance'))
    verify = ' '.join(t.get('verification_commands') or [])
    paths = t.get('paths') or []
    named = inp['main']['named_file_lines']
    named_lines = sum(v for v in named.values() if isinstance(v, int))
    acceptance = t.get('acceptance') or ''
    items = max(len(re.findall(r'[（(]\s*\d+\s*[)）]', acceptance)), acceptance.count('。'))
    return {
        'runtime': int(prefix.startswith(('runtime', 'application', 'domain', 'supervisor', 'stats', 'fix'))
                       or 'llvm-cov' in verify),
        'docs_only': int(bool(paths) and all(p.startswith(('docs/', '*.md', 'plugins/')) for p in paths)),
        'test_or_build': int(prefix.startswith(('test', 'ci', 'build', 'e2e'))),
        'log_chars': math.log10(1 + len(text)),
        'acceptance_items': items,
        'deps': len(t.get('dependencies') or []),
        'llvm_cov': int('llvm-cov' in verify),
        'e2e': int('e2e' in (t.get('required_evidence') or [])),
        'adr': int(bool(re.search(r'ADR-\d{4}（docs/adr/|ADR を書', text))),
        'migration': int('migration' in text.lower()),
        'named_files': len(named),
        'log_named_lines': math.log10(1 + named_lines),
        'predecessor_chars': sum(len(p.get('summary') or '') for p in inp.get('predecessors', [])),
        'earlier_runs': len(inp.get('earlier_runs_of_task', [])),
    }


def rule_scores(f):
    """Hand-set weights, chosen from what the planner already believed (runtime > test > docs,
    longer bodies and more acceptance items are heavier), not fitted to the outcomes."""
    heavy = (2.0 * f['runtime'] + 1.0 * f['test_or_build'] - 1.0 * f['docs_only'] + 1.5 * f['log_chars']
             + 0.15 * min(f['acceptance_items'], 12) + 0.5 * f['migration'] + 0.3 * f['adr']
             + 0.3 * f['log_named_lines'] + 0.1 * min(f['named_files'], 10))
    rework = heavy + 0.3 * min(f['deps'], 5) + 0.5 * f['e2e'] + 0.7 * f['earlier_runs']
    return heavy, rework


# ---------- LLM ----------

def call(model, effort, prompt, cwd):
    args = ['claude', '-p', '--model', model, '--strict-mcp-config', '--tools', '',
            '--system-prompt', SYSTEM, '--output-format', 'json', '--no-session-persistence']
    if effort:
        args += ['--effort', effort]
    env = {k: os.environ[k] for k in ('HOME', 'PATH', 'USER', 'LANG', 'TERM') if k in os.environ}
    started = time.time()
    proc = subprocess.run(args, input=prompt, cwd=cwd, env=env, capture_output=True, text=True, timeout=900)
    wall = time.time() - started
    res = json.loads(proc.stdout)
    text = res.get('result') or ''
    m = re.search(r'\{.*\}', text, re.S)
    parsed = json.loads(m.group(0)) if m else None
    usage = res.get('usage') or {}
    return {'prediction': parsed, 'raw': text, 'is_error': res.get('is_error'), 'wall_secs': round(wall, 1),
            'duration_ms': res.get('duration_ms'), 'cost_usd': res.get('total_cost_usd'),
            'input_tokens': usage.get('input_tokens', 0) + usage.get('cache_read_input_tokens', 0)
            + usage.get('cache_creation_input_tokens', 0),
            'output_tokens': usage.get('output_tokens', 0)}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('candidate', choices=['rules', *MODELS])
    ap.add_argument('--inputs', default='a,b')
    ap.add_argument('--only-sample', action='store_true', help='skip the rework supplement')
    ap.add_argument('--jobs', type=int, default=2)
    args = ap.parse_args()
    items = json.loads((OUT / 'dataset.json').read_text())['items']
    if args.only_sample:
        items = [i for i in items if not i['supplement']]
    dest = OUT / 'predictions' / args.candidate
    dest.mkdir(parents=True, exist_ok=True)
    empty = OUT / 'empty'  # no CLAUDE.md, no project memory
    empty.mkdir(exist_ok=True)

    todo = []
    for item in items:
        for which in args.inputs.split(','):
            path = dest / f"{item['run_id']}_{which}.json"
            if path.exists():
                continue
            todo.append((item, which, path))

    def work(job):
        item, which, path = job
        inp = item[which]
        if args.candidate == 'rules':
            started = time.time()
            f = features(inp)
            heavy, rework = rule_scores(f)
            result = {'features': f, 'heavy_score': heavy, 'rework_score': rework,
                      'wall_secs': round(time.time() - started, 4), 'input_tokens': 0, 'output_tokens': 0}
        else:
            model, effort = MODELS[args.candidate]
            for attempt in range(3):
                try:
                    result = call(model, effort, prompt_for(inp, which), empty)
                    if result['prediction'] is not None:
                        break
                except (subprocess.SubprocessError, json.JSONDecodeError) as e:
                    result = {'prediction': None, 'error': str(e)}
                time.sleep(5)
        path.write_text(json.dumps(result, ensure_ascii=False, indent=1))
        print(f"{args.candidate} task {item['task_id']} {which} done", file=sys.stderr)

    with ThreadPoolExecutor(max_workers=1 if args.candidate == 'rules' else min(args.jobs, 2)) as pool:
        list(pool.map(work, todo))


if __name__ == '__main__':
    main()
