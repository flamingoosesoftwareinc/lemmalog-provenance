//! Presentation models and pure views for CLI search results.

use crate::search::SearchOutput;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SearchFormat {
    Canonical,
    Text,
    Json,
}

impl SearchFormat {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "canonical" => Ok(Self::Canonical),
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            _ => Err(format!(
                "--format requires canonical, text, or json, got {value:?}"
            )),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SearchCard {
    relationship: String,
    scope: String,
    provenance: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Model {
    cards: Vec<SearchCard>,
    truncated: bool,
    limit: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Msg {
    Results {
        rows: Vec<String>,
        truncated: bool,
        limit: usize,
    },
}

pub(crate) fn render_search(
    output: &SearchOutput,
    format: SearchFormat,
    limit: usize,
) -> Result<String, String> {
    if output.rows.is_empty() {
        return Ok(match format {
            SearchFormat::Json => {
                format!("{{\"results\":[],\"truncated\":false,\"limit\":{limit}}}")
            }
            SearchFormat::Canonical | SearchFormat::Text => "(no answers)".to_string(),
        });
    }
    let model = update(
        Model {
            cards: Vec::new(),
            truncated: false,
            limit,
        },
        Msg::Results {
            rows: output.rows.clone(),
            truncated: output.truncated,
            limit,
        },
    )?;
    Ok(match format {
        SearchFormat::Canonical => canonical(&model),
        SearchFormat::Text => text(&model),
        SearchFormat::Json => json(&model),
    })
}

fn update(mut model: Model, message: Msg) -> Result<Model, String> {
    match message {
        Msg::Results {
            rows,
            truncated,
            limit,
        } => {
            model.cards = rows
                .into_iter()
                .map(|row| parse_row(&row))
                .collect::<Result<Vec<_>, _>>()?;
            model.truncated = truncated;
            model.limit = limit;
        }
    }
    Ok(model)
}

fn parse_row(row: &str) -> Result<SearchCard, String> {
    let mut fields = row.splitn(3, '\t');
    let scope = fields
        .next()
        .ok_or_else(|| "search row is missing scope".to_string())?;
    let relationship = fields
        .next()
        .ok_or_else(|| "search row is missing relationship".to_string())?;
    let provenance = fields
        .next()
        .ok_or_else(|| "search row is missing provenance".to_string())?
        .strip_prefix("provenance=")
        .ok_or_else(|| "search row has invalid provenance".to_string())?;
    Ok(SearchCard {
        relationship: relationship.to_string(),
        scope: scope.to_string(),
        provenance: provenance.to_string(),
    })
}

fn canonical(model: &Model) -> String {
    let mut output = model
        .cards
        .iter()
        .map(|card| {
            format!(
                "{}\t{}\tprovenance={}",
                card.scope, card.relationship, card.provenance
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    append_truncation(&mut output, model);
    output
}

fn text(model: &Model) -> String {
    let mut output = format!("Search results ({})\n", model.cards.len());
    for (index, card) in model.cards.iter().enumerate() {
        if index > 0 {
            output.push('\n');
        }
        output.push_str("════════════════════════════════════════════════════════════════\n");
        output.push_str("Relationship\n  ");
        output.push_str(&card.relationship);
        output.push_str("\n────────────────────────────────────────────────────────────────\n");
        output.push_str("Scope\n  ");
        output.push_str(&scope_display(&card.scope));
        output.push_str("\n────────────────────────────────────────────────────────────────\n");
        output.push_str("Provenance\n");
        for reference in provenance_display(&card.provenance) {
            output.push_str("  ");
            output.push_str(&reference);
            output.push('\n');
        }
    }
    output.push_str("════════════════════════════════════════════════════════════════");
    append_truncation(&mut output, model);
    output
}

fn json(model: &Model) -> String {
    let cards = model
        .cards
        .iter()
        .map(|card| {
            let provenance = provenance_display(&card.provenance)
                .into_iter()
                .map(|reference| format!("\"{}\"", escape_json(&reference)))
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "{{\"relationship\":\"{}\",\"scope\":\"{}\",\"provenance\":[{}]}}",
                escape_json(&card.relationship),
                escape_json(&card.scope),
                provenance
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{\"results\":[{}],\"truncated\":{},\"limit\":{}}}",
        cards, model.truncated, model.limit
    )
}

fn scope_display(scope: &str) -> String {
    scope
        .split_once(':')
        .map(|(kind, identity)| format!("{kind} · {identity}"))
        .unwrap_or_else(|| scope.to_string())
}

fn provenance_display(provenance: &str) -> Vec<String> {
    let Some((episode, source)) = provenance.split_once(',') else {
        return if episode_reference(provenance) {
            vec![format!("internal episode: {provenance}")]
        } else {
            vec![provenance.to_string()]
        };
    };
    if episode_reference(episode) {
        vec![
            format!("internal episode: {episode}"),
            format!("source: {source}"),
        ]
    } else {
        vec![provenance.to_string()]
    }
}

fn episode_reference(value: &str) -> bool {
    value.starts_with("ep")
        && value.len() > 2
        && value[2..]
            .chars()
            .all(|character| character.is_ascii_digit())
}

fn escape_json(value: &str) -> String {
    let mut escaped = String::new();
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character.is_control() => {
                escaped.push_str(&format!("\\u{:04x}", character as u32))
            }
            character => escaped.push(character),
        }
    }
    escaped
}

fn append_truncation(output: &mut String, model: &Model) {
    if model.truncated {
        output.push_str(&format!(
            "\ntruncated: limit ({} shown, more matched)",
            model.limit
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output() -> SearchOutput {
        SearchOutput {
            rows: vec![
                "repository:git@github.com:example/service.git\talpha --describes--> Search\tprovenance=ep1,https://github.com/example/service/blob/abc/README.md#L2-L4".to_string(),
                "workspace\tbilling --owns--> ledger\tprovenance=ep2".to_string(),
            ],
            truncated: true,
            stats: Default::default(),
        }
    }

    #[test]
    fn text_view_preserves_hierarchy_and_full_source_url() {
        let rendered = render_search(&output(), SearchFormat::Text, 2).unwrap();
        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn json_view_is_structured() {
        let rendered = render_search(&output(), SearchFormat::Json, 2).unwrap();
        insta::assert_snapshot!(rendered);
    }

    #[test]
    fn empty_json_view_is_structured() {
        let output = SearchOutput {
            rows: Vec::new(),
            truncated: false,
            stats: Default::default(),
        };
        assert_eq!(
            render_search(&output, SearchFormat::Json, 7).unwrap(),
            "{\"results\":[],\"truncated\":false,\"limit\":7}"
        );
    }

    #[test]
    fn canonical_view_is_compatible() {
        let rendered = render_search(&output(), SearchFormat::Canonical, 2).unwrap();
        insta::assert_snapshot!(rendered);
    }
}
