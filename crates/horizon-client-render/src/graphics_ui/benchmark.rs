use super::{Group, Item, Page, Row, Sheet, Value, lang};
use std::sync::Arc;

pub(super) const NAME: &str = "Graphics Benchmark";
pub(super) const ABOUT: &str =
    "Find which graphics settings run best on this computer. Results stay on this computer.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BenchmarkAction {
    Quick,
    Thorough,
    Apply,
    Undo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BenchmarkRow {
    Action(BenchmarkAction),
    Result(usize),
    Status,
    Change(usize),
}

#[derive(Clone, Debug, Default)]
pub struct BenchmarkView {
    pub status: &'static str,
    pub can_start: bool,
    pub can_apply: bool,
    pub can_undo: bool,
    pub results: Vec<BenchmarkResult>,
    pub changes: Vec<BenchmarkChange>,
}

#[derive(Clone, Debug)]
pub struct BenchmarkResult {
    pub group: &'static str,
    pub candidate: String,
    pub profile: String,
    pub outcome: BenchmarkOutcome,
}

#[derive(Clone, Debug)]
pub enum BenchmarkOutcome {
    Pending,
    Failed(&'static str),
    Measured {
        average: f64,
        low: f64,
        load_ms: f64,
        score: f64,
    },
}

#[derive(Clone, Debug)]
pub struct BenchmarkChange {
    pub setting: &'static str,
    pub before: String,
    pub after: String,
}

pub(super) fn sheet(view: &Arc<BenchmarkView>) -> Sheet<Page, Item> {
    use BenchmarkAction::*;
    let action = |which, name, help, id, live| {
        let mut row = Row::new(
            Item::Benchmark(BenchmarkRow::Action(which)),
            lang::get(name),
            Value::Press,
        )
        .about(lang::get(help))
        .live(live);
        row.agent_id = Some(id);
        row
    };
    let prefixed = |id, name: &str, value| Row::new(Item::Benchmark(id), lang::get(name), value);
    let mut controls = vec![
        prefixed(
            BenchmarkRow::Status,
            "Benchmark status",
            Value::Read(lang::get(view.status)),
        ),
        action(
            Quick,
            "Find my best settings",
            "Run each fixed scene once per candidate. This can take several minutes.",
            "graphics-benchmark-quick",
            view.can_start,
        ),
        action(
            Thorough,
            "Run thorough benchmark",
            "Measure each fixed scene twice per candidate. This takes longer.",
            "graphics-benchmark-thorough",
            view.can_start,
        ),
        action(
            Apply,
            "Apply recommended settings",
            "Only the settings shown below change. A different graphics API takes effect the next time you start Horizon.",
            "graphics-benchmark-apply",
            view.can_apply,
        ),
        action(
            Undo,
            "Undo benchmark settings",
            "Restore the settings from before the last recommendation was applied.",
            "graphics-benchmark-undo",
            view.can_undo,
        ),
    ];
    if !view.changes.is_empty() {
        controls.extend(view.changes.iter().enumerate().map(|(i, change)| {
            prefixed(
                BenchmarkRow::Change(i),
                change.setting,
                Value::Read(lang::tr("%s → %s", &[&change.before, &change.after])),
            )
        }));
    }
    let results = view
        .results
        .iter()
        .enumerate()
        .map(|(i, result)| {
            let value = match result.outcome {
                BenchmarkOutcome::Pending => lang::get("Measuring…"),
                BenchmarkOutcome::Failed(reason) => lang::get(reason),
                BenchmarkOutcome::Measured {
                    average,
                    low,
                    load_ms,
                    score,
                } => lang::tr(
                    "%s FPS · %s low · %s ms · %s score",
                    &[
                        &format!("{average:.1}"),
                        &format!("{low:.1}"),
                        &format!("{load_ms:.0}"),
                        &format!("{score:.3}"),
                    ],
                ),
            };
            Row::new(
                Item::Benchmark(BenchmarkRow::Result(i)),
                lang::tr("%s: %s", &[&lang::get(result.group), &result.candidate]),
                Value::Read(value),
            )
            .about(lang::tr(
                "Settings: %s. Results: average FPS, 1%% low FPS, loading time, and relative score.",
                &[&result.profile],
            ))
        })
        .collect();
    Sheet {
        id: Page::Benchmark,
        name: lang::get(NAME),
        about: lang::get(ABOUT),
        warning: None,
        groups: vec![
            Group {
                name: "Benchmark controls",
                rows: controls,
            },
            Group {
                name: "Trial comparison",
                rows: results,
            },
        ],
        listed: true,
        heading: None,
    }
}
