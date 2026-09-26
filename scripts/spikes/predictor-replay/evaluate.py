#!/usr/bin/env python3
"""Score the predictions against the outcomes and write summary.json / summary.csv.

Heaviness metrics use the stratified sample only (supplement=False). The rework AUC uses the
sample plus the rework supplement (every other reworked run of the pool), since the sample alone
holds few reworked runs; AUC does not depend on the share of positives.

Usage: python3 evaluate.py [--out DIR]   (pure Python, no numpy)
"""

import argparse
import csv
import json
import math
from pathlib import Path
import random
import statistics as st
import sys

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import predict  # noqa: E402

CANDIDATES = ['rules', 'rules-loo', 'haiku', 'sonnet', 'opus-low', 'opus-high']
SIZE = {'S': 0, 'M': 1, 'L': 2}


def ranks(xs):
    order = sorted(range(len(xs)), key=lambda i: xs[i])
    r = [0.0] * len(xs)
    i = 0
    while i < len(order):
        j = i
        while j + 1 < len(order) and xs[order[j + 1]] == xs[order[i]]:
            j += 1
        for k in range(i, j + 1):
            r[order[k]] = (i + j) / 2 + 1
        i = j + 1
    return r


def pearson(x, y):
    mx, my = st.mean(x), st.mean(y)
    sx = math.sqrt(sum((a - mx) ** 2 for a in x))
    sy = math.sqrt(sum((b - my) ** 2 for b in y))
    return sum((a - mx) * (b - my) for a, b in zip(x, y)) / (sx * sy) if sx and sy else float('nan')


def spearman(x, y):
    return pearson(ranks(x), ranks(y))


def spearman_ci(x, y, n=2000, seed=0):
    rng = random.Random(seed)
    vals = []
    for _ in range(n):
        idx = [rng.randrange(len(x)) for _ in x]
        v = spearman([x[i] for i in idx], [y[i] for i in idx])
        if not math.isnan(v):
            vals.append(v)
    vals.sort()
    return vals[int(0.05 * len(vals))], vals[int(0.95 * len(vals)) - 1]


def auc(scores, labels):
    pos = [s for s, l in zip(scores, labels) if l]
    neg = [s for s, l in zip(scores, labels) if not l]
    if not pos or not neg:
        return float('nan')
    wins = sum(1.0 if p > q else 0.5 if p == q else 0.0 for p in pos for q in neg)
    return wins / (len(pos) * len(neg))


def top_third(scores, truth, predicted_heavy=None):
    """Precision / recall of the predicted heavy set against the actual top third of `truth`.

    Without `predicted_heavy`, the predicted set is the top third by score; ties at the cut count
    as expected hits (each tied run is in the set with the same probability), so tie order does
    not matter.
    """
    k = len(truth) // 3
    actual = set(sorted(range(len(truth)), key=lambda i: -truth[i])[:k])
    if predicted_heavy is not None:
        hit = len(actual & predicted_heavy)
        return (hit / len(predicted_heavy) if predicted_heavy else float('nan'), hit / len(actual))
    cut = sorted(scores, reverse=True)[k - 1]
    above = [i for i in range(len(scores)) if scores[i] > cut]
    tied = [i for i in range(len(scores)) if scores[i] == cut]
    hit = len(actual & set(above)) + (k - len(above)) * len(actual & set(tied)) / len(tied)
    return hit / k, hit / len(actual)


def solve(a, b):
    """Gaussian elimination for the small normal equations."""
    n = len(a)
    m = [row[:] + [b[i]] for i, row in enumerate(a)]
    for c in range(n):
        p = max(range(c, n), key=lambda r: abs(m[r][c]))
        m[c], m[p] = m[p], m[c]
        for r in range(n):
            if r != c and m[c][c]:
                f = m[r][c] / m[c][c]
                m[r] = [x - f * y for x, y in zip(m[r], m[c])]
    return [m[i][n] / m[i][i] if m[i][i] else 0.0 for i in range(n)]


def ridge_loo(rows, ys, lam=1.0):
    """Leave-one-out predictions of a ridge regression (features standardised on the training part)."""
    preds = []
    for held in range(len(rows)):
        train = [i for i in range(len(rows)) if i != held]
        cols = len(rows[0])
        mu = [st.mean(rows[i][c] for i in train) for c in range(cols)]
        sd = [st.pstdev([rows[i][c] for i in train]) or 1.0 for c in range(cols)]
        z = lambda r: [(r[c] - mu[c]) / sd[c] for c in range(cols)]  # noqa: E731
        xs = [z(rows[i]) for i in train]
        ym = st.mean(ys[i] for i in train)
        a = [[sum(x[p] * x[q] for x in xs) + (lam if p == q else 0) for q in range(cols)] for p in range(cols)]
        b = [sum(x[p] * (ys[i] - ym) for x, i in zip(xs, train)) for p in range(cols)]
        w = solve(a, b)
        preds.append(ym + sum(wi * zi for wi, zi in zip(w, z(rows[held]))))
    return preds


def load(out, candidate, item, which):
    path = out / 'predictions' / candidate / f"{item['run_id']}_{which}.json"
    return json.loads(path.read_text()) if path.exists() else None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--out', default=str(HERE / 'out'))
    args = ap.parse_args()
    out = Path(args.out)
    data = json.loads((out / 'dataset.json').read_text())
    items = data['items']
    sample = [i for i in items if not i['supplement']]
    rows = []
    calls = {}

    # Split rework by cause: a resume for a rebase conflict comes from other runs landing first and
    # a resume after the session was killed from outside comes from the host, not from the task.
    # `intrinsic` = a verification resume or a review revise / concern; runs whose only rework was
    # a conflict or a kill are left out of the intrinsic AUC.
    causes = {}
    for e in json.loads((out / 'events.json').read_text())['events']:
        if e['kind'] == 'resume_started':
            reason = e['payload'].get('reason', '')
            kind = ('conflict' if 'conflict' in reason else 'killed' if 'killed' in reason
                    else 'verification' if reason.startswith('verification command') else 'other')
            causes.setdefault(e['run_id'], set()).add(kind)
    for i in items:
        c = set(causes.get(i['run_id'], ()))
        if i['outcome']['review_verdict'] in ('revise', 'concern'):
            c.add('review')
        i['outcome']['rework_causes'] = sorted(c)
        i['outcome']['intrinsic'] = None if c and c <= {'conflict', 'killed'} else int(bool(c))

    # features for the fitted rule (leave-one-out on the sample, so no run sees its own outcome)
    feats = {}
    for which in ('a', 'b'):
        fs = [predict.features(i[which]) for i in sample]
        keys = sorted(fs[0])
        x = [[f[k] for k in keys] for f in fs]
        y = [math.log(i['outcome']['output_tokens']) for i in sample]
        for i, p in zip(sample, ridge_loo(x, y)):
            feats[(i['run_id'], which)] = p

    for cand in CANDIDATES:
        for which in ('a', 'b'):
            def score(item):
                if cand == 'rules-loo':
                    v = feats.get((item['run_id'], which))
                    return None if v is None else {'heavy': v, 'rework': None, 'size': None, 'cost': None}
                p = load(out, 'rules' if cand == 'rules' else cand, item, which)
                if p is None:
                    return None
                if cand == 'rules':
                    return {'heavy': p['heavy_score'], 'rework': p['rework_score'], 'size': None, 'cost': p}
                pred = p.get('prediction') or {}
                try:
                    heavy = float(pred['expected_output_tokens'])
                    rework = float(pred['rework_probability'])
                except (KeyError, TypeError, ValueError):
                    return None
                return {'heavy': heavy, 'rework': rework, 'size': pred.get('size'), 'cost': p}

            got = [(i, score(i)) for i in sample]
            got = [(i, s) for i, s in got if s]
            if len(got) < 10:
                continue
            tok = [i['outcome']['output_tokens'] for i, _ in got]
            secs = [i['outcome']['model_secs'] for i, _ in got]
            hv = [s['heavy'] for _, s in got]
            rho_t = spearman(hv, tok)
            ci = spearman_ci(hv, tok)
            prec, rec = top_third(hv, tok)
            row = {'candidate': cand, 'input': which, 'n': len(got),
                   'spearman_tokens': round(rho_t, 3), 'spearman_tokens_ci90': [round(c, 2) for c in ci],
                   'spearman_model_secs': round(spearman(hv, secs), 3),
                   'top3_precision': round(prec, 3), 'top3_recall': round(rec, 3)}
            if got[0][1]['size'] is not None:
                sizes = [SIZE.get(s['size'], 1) for _, s in got]
                heavy_set = {k for k, (_, s) in enumerate(got) if s['size'] == 'L'}
                p2, r2 = top_third(hv, tok, heavy_set)
                row.update({'spearman_size_tokens': round(spearman(sizes, tok), 3),
                            'L_precision': round(p2, 3), 'L_recall': round(r2, 3), 'L_count': len(heavy_set)})
            rw = [(i, score(i)) for i in items]
            rw = [(i, s) for i, s in rw if s and s['rework'] is not None]
            if rw:
                row['rework_auc'] = round(auc([s['rework'] for _, s in rw], [i['outcome']['rework'] for i, _ in rw]), 3)
                row['rework_n'] = f"{sum(i['outcome']['rework'] for i, _ in rw)}/{len(rw)}"
                ir = [(i, s) for i, s in rw if i['outcome']['intrinsic'] is not None]
                row['rework_auc_intrinsic'] = round(auc([s['rework'] for _, s in ir],
                                                        [i['outcome']['intrinsic'] for i, _ in ir]), 3)
                row['rework_auc_by_heavy'] = round(auc([s['heavy'] for _, s in rw],
                                                       [i['outcome']['rework'] for i, _ in rw]), 3)
                row['intrinsic_n'] = f"{sum(i['outcome']['intrinsic'] for i, _ in ir)}/{len(ir)}"
                smp = [(i, s) for i, s in rw if not i['supplement']]
                row['rework_auc_sample_only'] = round(auc([s['rework'] for _, s in smp],
                                                          [i['outcome']['rework'] for i, _ in smp]), 3)
            costs = [s['cost'] for _, s in rw if s['cost']] if rw else []
            if costs and cand != 'rules':
                row.update({'calls': len(costs),
                            'mean_input_tokens': round(st.mean(c['input_tokens'] for c in costs)),
                            'mean_output_tokens': round(st.mean(c['output_tokens'] for c in costs)),
                            'mean_cost_usd': round(st.mean(c.get('cost_usd') or 0 for c in costs), 4),
                            'median_wall_secs': round(st.median(c['wall_secs'] for c in costs), 1)})
                calls[(cand, which)] = costs
            elif cand == 'rules':
                row.update({'calls': 0, 'mean_input_tokens': 0, 'mean_output_tokens': 0, 'mean_cost_usd': 0,
                            'median_wall_secs': 0})
            rows.append(row)

    # paired bootstrap of Spearman differences on the same runs: (b) - (a) per candidate, and
    # each candidate's (b) against Sonnet (b)
    def heavy_of(cand, which, item):
        if cand == 'rules-loo':
            return feats.get((item['run_id'], which))
        p = load(out, cand, item, which)
        if p is None:
            return None
        if cand == 'rules':
            return p['heavy_score']
        v = (p.get('prediction') or {}).get('expected_output_tokens')
        return None if v is None else float(v)

    def paired(x1, x2, y, n=2000, seed=1):
        rng = random.Random(seed)
        vals = []
        for _ in range(n):
            idx = [rng.randrange(len(y)) for _ in y]
            yy = [y[i] for i in idx]
            vals.append(spearman([x2[i] for i in idx], yy) - spearman([x1[i] for i in idx], yy))
        vals = sorted(v for v in vals if not math.isnan(v))
        return [round(spearman(x2, y) - spearman(x1, y), 3), round(vals[int(0.05 * len(vals))], 2),
                round(vals[int(0.95 * len(vals)) - 1], 2)]

    diffs = {}
    for cand in CANDIDATES:
        pairs = [(heavy_of(cand, 'a', i), heavy_of(cand, 'b', i), i['outcome']['output_tokens']) for i in sample]
        pairs = [p for p in pairs if p[0] is not None and p[1] is not None]
        if len(pairs) >= 10:
            diffs[f'{cand} b-a'] = paired(*zip(*pairs))
        if cand != 'sonnet':
            pairs = [(heavy_of('sonnet', 'b', i), heavy_of(cand, 'b', i), i['outcome']['output_tokens']) for i in sample]
            pairs = [p for p in pairs if p[0] is not None and p[1] is not None]
            if len(pairs) >= 10:
                diffs[f'{cand}(b) - sonnet(b)'] = paired(*zip(*pairs))

    # references: after-the-fact size (not available before the run) and the measure agreement
    tok = [i['outcome']['output_tokens'] for i in sample]
    ref = {
        'n_sample': len(sample), 'n_with_supplement': len(items),
        'reworked_sample': sum(i['outcome']['rework'] for i in sample),
        'reworked_all': sum(i['outcome']['rework'] for i in items),
        'spearman_changed_lines_tokens': round(spearman([i['outcome']['changed']['lines'] for i in sample], tok), 3),
        'spearman_task_id_tokens': round(spearman([i['task_id'] for i in sample], tok), 3),
        'spearman_model_secs_tokens': round(spearman([i['outcome']['model_secs'] for i in sample], tok), 3),
        'median_output_tokens': st.median(tok),
        'median_output_tokens_per_line_sum': st.median(i['outcome']['output_tokens_per_line_sum'] for i in sample),
        'median_model_secs': st.median(i['outcome']['model_secs'] for i in sample),
        'a_b_task_body_differs': sum(i['a']['task'] != i['b']['task'] for i in sample),
        'b_with_predecessor_summary': sum(bool(i['b'].get('predecessors')) for i in sample),
        'total_llm_calls': sum(len(v) for v in calls.values()),
        'total_llm_input_tokens': sum(c['input_tokens'] for v in calls.values() for c in v),
        'total_llm_output_tokens': sum(c['output_tokens'] for v in calls.values() for c in v),
        'total_llm_cost_usd': round(sum(c.get('cost_usd') or 0 for v in calls.values() for c in v), 2),
    }
    ref['rework_causes'] = {}
    for i in items:
        if i['outcome']['rework']:
            k = '+'.join(i['outcome']['rework_causes'])
            ref['rework_causes'][k] = ref['rework_causes'].get(k, 0) + 1
    calib = {}
    for cand in ('haiku', 'sonnet', 'opus-low', 'opus-high'):
        ps = [(heavy_of(cand, 'b', i), i['outcome']['output_tokens']) for i in sample]
        ps = [(p, t) for p, t in ps if p is not None]
        if ps:
            calib[cand] = {'median_predicted': st.median(p for p, _ in ps),
                           'ratio_of_medians': round(st.median(p for p, _ in ps) / st.median(t for _, t in ps), 2),
                           'median_of_ratios': round(st.median(p / t for p, t in ps), 2),
                           'mean_abs_log10_error': round(st.mean(abs(math.log10(p / t)) for p, t in ps), 2)}
    ref['calibration_b'] = calib
    ref['a_b_text_differs'] = sum({k: v for k, v in i['a']['task'].items() if k != 'dependencies'}
                                  != {k: v for k, v in i['b']['task'].items() if k != 'dependencies'} for i in sample)
    ref['integrate_verify_failed_runs'] = sum(i['outcome']['integrate_verify_failures'] > 0 for i in items)
    ref['integrate_verify_failed_without_rework'] = sum(
        i['outcome']['integrate_verify_failures'] > 0 and not i['outcome']['rework'] for i in items)
    both = [i for i in sample if i['outcome'].get('stats_model_secs')]
    if len(both) > 5:
        ref['spearman_model_secs_vs_stats_model'] = round(spearman(
            [i['outcome']['model_secs'] for i in both], [i['outcome']['stats_model_secs'] for i in both]), 3)
    by_cat = {}
    for i in sample:
        by_cat.setdefault(i['category'], []).append(i['outcome']['output_tokens'])
    ref['median_tokens_by_category'] = {k: [len(v), st.median(v)] for k, v in sorted(by_cat.items())}

    (HERE / 'summary.json').write_text(json.dumps({'reference': ref, 'spearman_differences_ci90': diffs, 'rows': rows}, ensure_ascii=False, indent=1) + '\n')
    keys = []
    for r in rows:
        keys += [k for k in r if k not in keys]
    with open(HERE / 'summary.csv', 'w', newline='') as f:
        w = csv.DictWriter(f, fieldnames=keys)
        w.writeheader()
        w.writerows(rows)
    print(json.dumps(ref, ensure_ascii=False, indent=1))
    for r in rows:
        print(r)


if __name__ == '__main__':
    main()
