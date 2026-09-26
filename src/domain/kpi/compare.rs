//! The comparison across a change (ADR-0051 decisions 14–16): a window
//! before the mark and one after it, every other mark in and between them
//! listed next to the numbers (`confounders`), both split by the kind,
//! `parallel`, the load band and the build, and marks too close to split
//! (fewer finished runs between them than `min_samples`) taken as one
//! overlapping change. Nothing is removed automatically: a person narrows
//! the windows with `--compare A..B,C..D`.
use std::collections::BTreeMap;

use serde::Serialize;

use super::{
    COMPARE_AXES, Change, CompareSpec, DAY_MS, KpiQuery, Kpis, Measure, cursor_ms, window::Context,
};
use crate::domain::{
    marks::{self, MARK_RETRACTED, Mark},
    stats::{Cursor, timestamp_millis},
};

/// The kind a comparison's summary is made for without `--kind`: work
/// differs by an order of magnitude between kinds (decision 15).
pub const SUMMARY_KIND: &str = "runtime";

/// One side of a comparison.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WindowSpan {
    pub start: String,
    pub end: String,
    /// The window reaches past now.
    pub partial: bool,
    /// The runs that finished in it.
    pub runs: usize,
}

/// The change split at, when `--compare` names a mark or a time.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Split {
    /// Where the window before ends and the one after starts.
    pub start: String,
    pub end: String,
    /// The marks of the change: one, or the overlapping ones taken
    /// together; none for a time no mark took effect at.
    pub marks: Vec<Mark>,
    /// False when the change is overlapping marks: this comparison cannot
    /// tell them apart.
    pub separable: bool,
}

/// A mark in or between the windows other than the change compared.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Confounder {
    /// `before`, `between` or `after`.
    pub position: &'static str,
    #[serde(flatten)]
    pub mark: Mark,
}

/// One KPI of one stratum on both sides.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Side {
    pub before: Measure,
    pub after: Measure,
    #[serde(flatten)]
    pub change: Change,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Comparison {
    pub split: Option<Split>,
    pub before: WindowSpan,
    pub after: WindowSpan,
    pub confounders: Vec<Confounder>,
    /// The groups of overlapping marks in the range, each taken as one change.
    pub overlapping: Vec<Vec<Mark>>,
    /// Per KPI, per stratum (`all`, `kind=`, `parallel=`, `load=`, `build=`).
    pub strata: Kpis<Side>,
    /// The times of the work (`lead_time`, `phase.*`, `land_phase.*`) for
    /// each kind the summary is made for, by kind.
    pub summary: BTreeMap<String, BTreeMap<String, Side>>,
}

/// The marks that split KPIs, by when they took effect: neither a
/// retracted mark nor a retraction.
fn splitting(marks: &[Mark]) -> Vec<&Mark> {
    marks
        .iter()
        .filter(|mark| mark.retracted_by.is_none() && mark.kind != MARK_RETRACTED)
        .collect()
}

fn mark_ms(mark: &Mark) -> i64 {
    timestamp_millis(&mark.at).unwrap_or(i64::MIN)
}

/// The marks grouped into changes (decision 16): a mark joins the group of
/// the one before it when fewer than `min_samples` runs finished after that
/// one up to it. `finishes` are the runs' finish times, ascending.
pub fn overlapping_groups(marks: &[Mark], finishes: &[i64], min_samples: usize) -> Vec<Vec<Mark>> {
    let mut groups: Vec<Vec<Mark>> = Vec::new();
    for mark in splitting(marks) {
        let at = mark_ms(mark);
        match groups.last_mut() {
            Some(group)
                if {
                    let previous = mark_ms(group.last().expect("groups are not empty"));
                    let between = finishes.partition_point(|&t| t <= at)
                        - finishes.partition_point(|&t| t <= previous);
                    between < min_samples
                } =>
            {
                group.push(mark.clone());
            }
            _ => groups.push(vec![mark.clone()]),
        }
    }
    groups
}

/// The comparison `spec` asks for.
pub(super) fn compare(
    context: &Context<'_>,
    spec: CompareSpec,
    query: &KpiQuery,
    now_ms: i64,
) -> Result<Comparison, String> {
    let events = context.events;
    let min_samples = context.min_samples;
    let groups = overlapping_groups(&context.marks, &context.finishes, min_samples);
    let at = |cursor: Cursor| cursor_ms(cursor, events).ok_or("--compare names no event");
    let (split, before, after) = match spec {
        CompareSpec::At(cursor) => {
            let time = match cursor {
                // A person's mark of an earlier time splits where it took effect.
                Cursor::Event(id) => context
                    .marks
                    .iter()
                    .find(|mark| mark.id == Some(id))
                    .map_or_else(|| at(cursor), |mark| Ok(mark_ms(mark)))?,
                Cursor::Time(ms) => ms,
            };
            let group = groups.iter().find(|group| {
                group.iter().any(|mark| match cursor {
                    // A derived mark is named by the claim it was read off.
                    Cursor::Event(id) => {
                        mark.id == Some(id) || mark.detail["claim_event"] == id.as_i64()
                    }
                    Cursor::Time(ms) => mark_ms(mark) == ms,
                })
            });
            let (first, last) = group.map_or((time, time), |group| {
                (mark_ms(&group[0]), mark_ms(&group[group.len() - 1]))
            });
            let window = query.window_days.max(1) * DAY_MS;
            (
                Some(Split {
                    start: marks::utc_text(first),
                    end: marks::utc_text(last),
                    marks: group.cloned().unwrap_or_default(),
                    separable: group.is_none_or(|group| group.len() == 1),
                }),
                (first - window, first),
                (last, last + window),
            )
        }
        CompareSpec::Windows([(a, b), (c, d)]) => {
            let (before, after) = ((at(a)?, at(b)?), (at(c)?, at(d)?));
            if before.0 >= before.1 || after.0 >= after.1 {
                return Err("each --compare window must start before it ends".into());
            }
            if before.1 > after.0 {
                return Err("the first --compare window must end before the second starts".into());
            }
            (None, before, after)
        }
    };
    let in_split = |mark: &Mark| {
        split
            .as_ref()
            .is_some_and(|split| split.marks.iter().any(|kept| kept == mark))
    };
    let confounders = splitting(&context.marks)
        .into_iter()
        .filter(|mark| {
            let at = mark_ms(mark);
            at > before.0 && at <= after.1 && !in_split(mark)
        })
        .map(|mark| {
            let at = mark_ms(mark);
            Confounder {
                position: if at <= before.1 {
                    "before"
                } else if at > after.0 {
                    "after"
                } else {
                    "between"
                },
                mark: mark.clone(),
            }
        })
        .collect();
    let overlapping = groups
        .iter()
        .filter(|group| {
            group.len() > 1
                && group.iter().any(|mark| {
                    let at = mark_ms(mark);
                    at > before.0 && at <= after.1
                })
        })
        .cloned()
        .collect();
    let windows = [before, after].map(|(start, end)| context.window(start, end, &COMPARE_AXES));
    let span = |(start, end): (i64, i64), runs: usize| WindowSpan {
        start: marks::utc_text(start),
        end: marks::utc_text(end),
        partial: end > now_ms,
        runs,
    };
    let empty = Measure::default();
    let after_partial = after.1 > now_ms;
    let mut strata: Kpis<Side> = BTreeMap::new();
    for (name, after_strata) in &windows[1].kpis {
        let before_strata = windows[0].kpis.get(name);
        let keys = after_strata
            .keys()
            .chain(before_strata.into_iter().flat_map(|s| s.keys()));
        for stratum in keys {
            let before = before_strata.and_then(|s| s.get(stratum));
            let after = after_strata.get(stratum).unwrap_or(&empty);
            strata.entry(name.clone()).or_default().insert(
                stratum.clone(),
                Side {
                    before: before.cloned().unwrap_or_default(),
                    after: after.clone(),
                    change: {
                        let mut change = Change::between(name, before, after, min_samples);
                        if after_partial {
                            change.not_over();
                        }
                        change
                    },
                },
            );
        }
    }
    let kinds = if query.kinds.is_empty() {
        vec![SUMMARY_KIND.to_owned()]
    } else {
        query.kinds.clone()
    };
    let summary = kinds
        .into_iter()
        .map(|kind| {
            let stratum = format!("kind={kind}");
            let kpis = strata
                .iter()
                .filter(|(name, _)| {
                    *name == "lead_time"
                        || name.starts_with("phase.")
                        || name.starts_with("land_phase.")
                })
                .filter_map(|(name, sides)| Some((name.clone(), sides.get(&stratum)?.clone())))
                .collect();
            (kind, kpis)
        })
        .collect();
    Ok(Comparison {
        split,
        before: span(before, windows[0].runs),
        after: span(after, windows[1].runs),
        confounders,
        overlapping,
        strata,
        summary,
    })
}
