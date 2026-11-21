use crate::print_info::{PrintInfo, Targets};

pub(super) struct Row {
    pub text: String,
    pub swatch: Option<[u8; 3]>,
}

fn decimal(n: f64) -> String {
    format!("{n:.2}")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .into()
}

fn temperature(name: &str, targets: &Targets) -> Option<String> {
    Some(match (targets.first, targets.later) {
        (Some(a), Some(b)) if a != b => format!("{name} {}→{}°C", decimal(a), decimal(b)),
        (_, Some(b)) => format!("{name} {}°C", decimal(b)),
        (Some(a), None) => format!("{name} INITIAL {}°C", decimal(a)),
        (None, None) => return None,
    })
}

pub(super) fn rows(info: &PrintInfo, columns: usize) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut push = |text: String, swatch| rows.push(Row { text, swatch });
    if let Some(s) = info.time {
        let time = if s >= 3600 {
            format!("{}H {}M", s / 3600, s / 60 % 60)
        } else if s >= 60 {
            format!("{}M", s / 60)
        } else {
            format!("{s}S")
        };
        push(
            format!(
                "{} {time}",
                if info.model_time {
                    "MODEL EST."
                } else {
                    "EST. TIME"
                }
            ),
            None,
        );
    }
    if info.filament.is_some() || info.consumption.is_some() || info.swatch.is_some() {
        let amount = info
            .consumption
            .map(|(v, u)| format!("{} {u}", decimal(v)))
            .unwrap_or_default();
        let label = "FILAMENT";
        let room = columns.saturating_sub(
            label.len()
                + 1
                + usize::from(info.swatch.is_some()) * 2
                + if amount.is_empty() {
                    0
                } else {
                    amount.len() + 3
                },
        );
        let mut parts = vec![label.to_owned()];
        if let Some(name) = &info.filament
            && room >= 4
        {
            parts.push(if name.chars().count() <= room {
                name.clone()
            } else {
                format!("{}...", name.chars().take(room - 3).collect::<String>())
            });
        }
        if !amount.is_empty() {
            if parts.len() > 1 {
                parts.push("/".into());
            }
            parts.push(amount);
        }
        push(parts.join(" "), info.swatch);
    }
    let mut line = String::new();
    for part in [
        temperature("NOZZLE", &info.nozzle),
        temperature("BED", &info.bed),
        info.chamber.map(|v| format!("CHAMBER {}°C", decimal(v))),
    ]
    .into_iter()
    .flatten()
    {
        if !line.is_empty() {
            if line.chars().count() + 2 + part.chars().count() > columns {
                push(std::mem::take(&mut line), None);
            } else {
                line.push_str("  ");
            }
        }
        line.push_str(&part);
    }
    if !line.is_empty() {
        push(line, None);
    }
    let layers = info.layers.map(|n| format!("LAYERS {n}"));
    let nominal = info.nominal.map(|n| format!("NOMINAL {} MM", decimal(n)));
    match (layers, nominal) {
        (Some(a), Some(b)) if a.len() + 2 + b.len() <= columns => push(format!("{a}  {b}"), None),
        (a, b) => {
            for text in [a, b].into_iter().flatten() {
                push(text, None);
            }
        }
    }
    if let Some(n) = info.top_z {
        push(format!("TOP LAYER Z {} MM", decimal(n)), None);
    }
    rows
}
