#!/usr/bin/env python3
"""Disposable interactive-Claude lifecycle probe; not the production runtime."""

import argparse
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import uuid


def run(*args, cwd=None):
    return subprocess.check_output(args, cwd=cwd, text=True).strip()


def save(path, value):
    temporary = path.with_suffix('.tmp')
    temporary.write_text(json.dumps(value, indent=2) + '\n')
    temporary.replace(path)


def prepare():
    root = Path(tempfile.mkdtemp(prefix='cmux-taskq-spike-')).resolve()
    repo, worktree = root / 'repo', root / 'worktree'
    repo.mkdir()
    run('git', 'init', '-b', 'main', str(repo))
    run('git', 'config', 'user.name', 'cmux-taskq spike', cwd=repo)
    run('git', 'config', 'user.email', 'spike@example.invalid', cwd=repo)
    (repo / 'greeting.py').write_text('def greeting(name):\n    return "Hello!"\n')
    (repo / 'test_greeting.py').write_text(
        'import unittest\nfrom greeting import greeting\n\n'
        'class GreetingTest(unittest.TestCase):\n'
        '    def test_name(self):\n'
        '        self.assertEqual(greeting("cmux"), "Hello, cmux!")\n'
    )
    (repo / '.gitignore').write_text('__pycache__/\n')
    run('git', 'add', '.', cwd=repo)
    run('git', 'commit', '-m', 'Add deliberately failing greeting fixture', cwd=repo)
    base = run('git', 'rev-parse', 'HEAD', cwd=repo)
    run('git', 'worktree', 'add', '-b', 'spike/greeting', str(worktree), cwd=repo)
    script = Path(__file__).resolve()
    state = {'run_id': str(uuid.uuid4()), 'root': str(root),
             'worktree': str(worktree), 'base_commit': base,
             'claude_version': run('claude', '--version'),
             'cmux_version': run('cmux', '--version')}
    save(root / 'run.json', state)
    submit = shlex.join([sys.executable, str(script), 'submit', str(root)])
    (root / 'prompt.txt').write_text(
        'This is a disposable cmux-taskq lifecycle smoke test. '
        'Work only in this worktree. Fix greeting.py so greeting("cmux") returns '
        '"Hello, cmux!". Do not change the test. Run python3 -m unittest -v, '
        'then commit the fix. Do not push, create agents, or close the workspace. '
        'After committing with a clean worktree, submit the completion receipt by running: '
        + submit + '\nThen report completion and wait for the operator to exit the session. '
        'E2E and subagent review are outside this lifecycle probe.\n'
    )
    command = shlex.join([sys.executable, str(script), 'session', str(root)])
    print(json.dumps({'root': str(root), 'launch_argv': [
        'cmux', '--json', '--id-format', 'uuids', 'new-workspace',
        '--name', 'taskq Claude lifecycle spike', '--cwd', str(worktree),
        '--command', command, '--focus', 'false']}, indent=2))


def session(root, state):
    # Inherit the terminal: redirecting stdout would change Claude's session mode.
    # A probe root represents exactly one attempt; never reuse stale receipts.
    with (root / 'wrapper.json').open('x') as marker:
        json.dump({'pid': os.getpid(), 'run_id': state['run_id']}, marker)
    try:
        result = subprocess.run([
            'claude', '--session-id', state['run_id'], '--add-dir', str(root),
            '--', (root / 'prompt.txt').read_text(),
        ], cwd=state['worktree'])
        save(root / 'exit.json', {'run_id': state['run_id'], 'exit_code': result.returncode})
    except Exception as error:
        save(root / 'exit.json', {'run_id': state['run_id'], 'error': str(error)})
        raise


def validate(root, state):
    worktree = state['worktree']
    commit = run('git', 'rev-parse', 'HEAD', cwd=worktree)
    if commit == state['base_commit']:
        raise ValueError('No result commit')
    if run('git', 'status', '--porcelain', '--untracked-files=all', cwd=worktree):
        raise ValueError('Worktree is not clean')
    run('git', 'merge-base', '--is-ancestor', state['base_commit'], commit, cwd=worktree)
    if run('git', 'diff', '--name-only', state['base_commit'], commit, cwd=worktree) != 'greeting.py':
        raise ValueError('Only greeting.py may change in this probe')
    result = subprocess.run([sys.executable, '-m', 'unittest', '-v'], cwd=worktree,
                            capture_output=True, text=True)
    (root / 'tests.log').write_text(result.stdout + result.stderr)
    if result.returncode:
        raise ValueError('Tests failed; see tests.log')
    return commit


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['prepare', 'session', 'submit', 'inspect'])
    parser.add_argument('root', nargs='?', type=Path)
    args = parser.parse_args()
    if args.action == 'prepare':
        prepare()
        return
    if args.root is None:
        parser.error('root is required for this action')
    root = args.root.resolve()
    state = json.loads((root / 'run.json').read_text())
    if args.action == 'session':
        session(root, state)
    elif args.action == 'submit':
        commit = validate(root, state)
        save(root / 'receipt.json', {
            'run_id': state['run_id'], 'result': 'succeeded', 'commit': commit,
            'tests': {'command': 'python3 -m unittest -v', 'exit_code': 0},
            'e2e': {'status': 'not_applicable', 'reason': 'Lifecycle fixture only'},
            'subagent_review': {'status': 'not_applicable', 'reason': 'Lifecycle fixture only'},
        })
        print('Receipt submitted; session exit is a separate operator action.')
    else:
        report = {'run_id': state['run_id'], 'root': str(root)}
        for name in ['wrapper', 'receipt', 'exit']:
            path = root / (name + '.json')
            report[name] = json.loads(path.read_text()) if path.exists() else None
        report['safe_to_close'] = False
        if report['receipt']:
            try:
                receipt = report['receipt']
                if receipt['run_id'] != state['run_id'] or receipt['result'] != 'succeeded':
                    raise ValueError('Receipt does not match this run')
                if receipt['commit'] != validate(root, state):
                    raise ValueError('Receipt commit does not match HEAD')
                report['validated'] = True
                report['safe_to_close'] = bool(
                    report['exit'] and report['exit'].get('exit_code') == 0
                    and report['exit'].get('run_id') == state['run_id'])
            except (ValueError, KeyError, subprocess.CalledProcessError) as error:
                report['validation_error'] = str(error)
        print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
