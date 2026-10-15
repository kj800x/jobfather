use maud::{Markup, html};
use serde::Serialize;

/// Named chip colors, mapped to theme styles in `styles.css`.
const PRESET_COLORS: &[&str] = &["neutral", "info", "success", "warning", "danger"];

/// A single chip emitted by a job via `/job-output/chips.json`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Chip {
    pub name: String,
    /// A JSON primitive: string, number, or boolean.
    pub value: serde_json::Value,
    /// Either a preset name or a normalized `#rrggbb` hex code.
    pub color: String,
}

/// Result of parsing a chips.json file: the valid chips, in order, plus
/// human-readable warnings for anything that was skipped or defaulted.
#[derive(Debug, Default)]
pub struct ParsedChips {
    pub chips: Vec<Chip>,
    pub warnings: Vec<String>,
}

/// Leniently parse chips.json. Invalid entries are skipped and reported in
/// `warnings` rather than failing the whole file.
pub fn parse(json: &str) -> ParsedChips {
    let mut parsed = ParsedChips::default();

    let entries = match serde_json::from_str::<serde_json::Value>(json) {
        Ok(serde_json::Value::Array(entries)) => entries,
        Ok(_) => {
            parsed
                .warnings
                .push("chips.json must be a JSON array".to_string());
            return parsed;
        }
        Err(e) => {
            parsed.warnings.push(format!("Invalid JSON: {}", e));
            return parsed;
        }
    };

    for (i, entry) in entries.iter().enumerate() {
        let Some(obj) = entry.as_object() else {
            parsed
                .warnings
                .push(format!("Chip {}: not an object, skipped", i));
            continue;
        };

        let Some(name) = obj.get("name").and_then(|n| n.as_str()) else {
            parsed
                .warnings
                .push(format!("Chip {}: missing string \"name\", skipped", i));
            continue;
        };

        let value = match obj.get("value") {
            Some(
                v @ (serde_json::Value::String(_)
                | serde_json::Value::Number(_)
                | serde_json::Value::Bool(_)),
            ) => v.clone(),
            _ => {
                parsed.warnings.push(format!(
                    "Chip {} ({}): \"value\" must be a string, number, or boolean, skipped",
                    i, name
                ));
                continue;
            }
        };

        let color = match obj.get("color") {
            None | Some(serde_json::Value::Null) => "neutral".to_string(),
            Some(serde_json::Value::String(c)) => match normalize_color(c) {
                Some(c) => c,
                None => {
                    parsed.warnings.push(format!(
                        "Chip {} ({}): unknown color \"{}\", using neutral",
                        i, name, c
                    ));
                    "neutral".to_string()
                }
            },
            Some(_) => {
                parsed.warnings.push(format!(
                    "Chip {} ({}): \"color\" must be a string, using neutral",
                    i, name
                ));
                "neutral".to_string()
            }
        };

        parsed.chips.push(Chip {
            name: name.to_string(),
            value,
            color,
        });
    }

    parsed
}

/// Parse chips.json, discarding warnings. Returns an empty list for `None`.
pub fn parse_opt(json: Option<&str>) -> Vec<Chip> {
    json.map(|j| parse(j).chips).unwrap_or_default()
}

/// Accepts a preset name (case-insensitive) or `#rgb` / `#rrggbb`.
/// Returns the preset name or a lowercase `#rrggbb`.
fn normalize_color(color: &str) -> Option<String> {
    let lower = color.trim().to_ascii_lowercase();
    if PRESET_COLORS.contains(&lower.as_str()) {
        return Some(lower);
    }
    let hex = lower.strip_prefix('#')?;
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    match hex.len() {
        6 => Some(lower),
        3 => Some(format!(
            "#{}",
            hex.chars().flat_map(|c| [c, c]).collect::<String>()
        )),
        _ => None,
    }
}

/// Pick dark or light text for a `#rrggbb` background based on relative luminance.
fn text_color_for(hex: &str) -> &'static str {
    let channel = |i: usize| {
        let v = u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0) as f64 / 255.0;
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    let luminance = 0.2126 * channel(1) + 0.7152 * channel(3) + 0.0722 * channel(5);
    if luminance > 0.4 {
        "#1f2328"
    } else {
        "#ffffff"
    }
}

pub fn display_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn render_chip(chip: &Chip) -> Markup {
    let value = display_value(&chip.value);
    let title = format!("{}: {}", chip.name, value);
    html! {
        @if chip.color.starts_with('#') {
            span class="chip" title=(title)
                style=(format!("background-color:{};color:{}", chip.color, text_color_for(&chip.color))) {
                span class="chip-name" { (&chip.name) }
                span class="chip-value" { (value) }
            }
        } @else {
            span class=(format!("chip chip-{}", chip.color)) title=(title) {
                span class="chip-name" { (&chip.name) }
                span class="chip-value" { (value) }
            }
        }
    }
}

/// Render chips as a wrapping row. Renders nothing for an empty list.
pub fn render(chips: &[Chip]) -> Markup {
    html! {
        @if !chips.is_empty() {
            span class="chips" {
                @for chip in chips {
                    (render_chip(chip))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_chips_in_order() {
        let parsed = parse(
            r##"[
                {"name": "coverage", "value": 87.3, "color": "success"},
                {"name": "env", "value": "staging"},
                {"name": "flaky", "value": true, "color": "WARNING"},
                {"name": "build", "value": "a1b2c3", "color": "#82F"}
            ]"##,
        );
        assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
        let names: Vec<_> = parsed.chips.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["coverage", "env", "flaky", "build"]);
        assert_eq!(parsed.chips[1].color, "neutral");
        assert_eq!(parsed.chips[2].color, "warning");
        assert_eq!(parsed.chips[3].color, "#8822ff");
        assert_eq!(display_value(&parsed.chips[0].value), "87.3");
        assert_eq!(display_value(&parsed.chips[2].value), "true");
    }

    #[test]
    fn skips_invalid_entries_with_warnings() {
        let parsed = parse(
            r#"[
                {"name": "ok", "value": 1},
                {"value": 2},
                {"name": "obj", "value": {"a": 1}},
                {"name": "nul", "value": null},
                {"name": "badcolor", "value": "x", "color": "chartreuse"},
                "not an object"
            ]"#,
        );
        let names: Vec<_> = parsed.chips.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["ok", "badcolor"]);
        assert_eq!(parsed.chips[1].color, "neutral");
        assert_eq!(parsed.warnings.len(), 5);
    }

    #[test]
    fn rejects_non_array_and_bad_json() {
        assert!(parse(r#"{"name": "x"}"#).chips.is_empty());
        assert_eq!(parse("not json").warnings.len(), 1);
    }

    #[test]
    fn rejects_hex_that_could_inject_style() {
        assert_eq!(normalize_color("#fff;background:url(x)"), None);
        assert_eq!(normalize_color("#12345"), None);
    }

    #[test]
    fn picks_readable_text_color() {
        assert_eq!(text_color_for("#ffffff"), "#1f2328");
        assert_eq!(text_color_for("#000000"), "#ffffff");
        assert_eq!(text_color_for("#8250df"), "#ffffff");
    }
}
