//! What an API call would cost at Anthropic's list prices, from the usage the transcript
//! records of it, as <https://platform.claude.com/docs/en/about-claude/pricing> lists
//! them. A plan's limits are spent by rules Anthropic does not publish, so the cost only ranks
//! turns against each other.

use serde::Deserialize;

/// Dollars per million tokens. A write to the 5-minute cache costs 1.25 times the input
/// and one to the 1-hour cache twice it, on every model.
struct Price {
    input: f64,
    output: f64,
    cache_read: f64,
}

/// The price of a model, by its id without a date suffix.
fn price(model: &str) -> Option<Price> {
    let model = match model.rsplit_once('-') {
        Some((model, date)) if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) => model,
        _ => model,
    };
    let (input, output, cache_read) = match model {
        "claude-fable-5-2" | "claude-mythos-5-2" => (8.0, 40.0, 0.2),
        "claude-fable-5-1" | "claude-mythos-5-1" => (10.0, 50.0, 0.25),
        "claude-fable-5" => (10.0, 50.0, 1.0),
        "claude-opus-5-5" => (4.0, 20.0, 0.2),
        "claude-opus-5" | "claude-opus-4-8" => (5.0, 25.0, 0.5),
        "claude-sonnet-5-5" => (2.0, 10.0, 0.1),
        "claude-sonnet-5" => (2.0, 10.0, 0.2),
        "claude-haiku-5-5" => (0.1, 0.5, 0.01),
        "claude-haiku-4-5" => (1.0, 5.0, 0.1),
        _ => return None,
    };
    Some(Price {
        input,
        output,
        cache_read,
    })
}

/// The usage an assistant entry records of the call that wrote it.
#[derive(Deserialize)]
pub(super) struct Billed {
    input_tokens: u64,
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    cache_creation: Option<Written>,
    server_tool_use: Option<Tools>,
    speed: Option<String>,
    inference_geo: Option<String>,
}

#[derive(Deserialize)]
struct Written {
    ephemeral_5m_input_tokens: u64,
    ephemeral_1h_input_tokens: u64,
}

#[derive(Deserialize)]
struct Tools {
    #[serde(default)]
    web_search_requests: u64,
}

impl Billed {
    /// The call's cost in dollars on `model`, which has no price when it is unknown.
    #[expect(
        clippy::cast_precision_loss,
        reason = "the tokens of a call stay far below 2^52"
    )]
    pub(super) fn cost(&self, model: &str) -> Option<f64> {
        let mut price = price(model)?;
        let prompt =
            self.input_tokens + self.cache_read_input_tokens + self.cache_creation_input_tokens;
        // Haiku 5.5 charges five times as much for a call whose prompt is over 100,000 tokens.
        if model.starts_with("claude-haiku-5-5") && prompt > 100_000 {
            price = Price {
                input: price.input * 5.0,
                output: price.output * 5.0,
                cache_read: price.cache_read * 5.0,
            };
        }
        let (short, long) =
            self.cache_creation
                .as_ref()
                .map_or((self.cache_creation_input_tokens, 0), |written| {
                    (
                        written.ephemeral_5m_input_tokens,
                        written.ephemeral_1h_input_tokens,
                    )
                });
        let tokens = self.input_tokens as f64 * price.input
            + short as f64 * price.input * 1.25
            + long as f64 * price.input * 2.0
            + self.cache_read_input_tokens as f64 * price.cache_read
            + self.output_tokens as f64 * price.output;
        // Fast mode doubles every price of the models that have it.
        let speed = if self.speed.as_deref() == Some("fast") {
            2.0
        } else {
            1.0
        };
        let geo = if self.inference_geo.as_deref() == Some("us") {
            1.1
        } else {
            1.0
        };
        let searches = self
            .server_tool_use
            .as_ref()
            .map_or(0, |tools| tools.web_search_requests);
        Some(tokens / 1e6 * speed * geo + searches as f64 * 0.01)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn cost(model: &str, usage: &serde_json::Value) -> Option<f64> {
        let billed: Billed = serde_json::from_value(usage.clone()).expect("a usage");
        billed
            .cost(model)
            .map(|dollars| (dollars * 1e6).round() / 1e6)
    }

    #[test]
    fn a_call_costs_each_kind_of_token_at_its_price() {
        let usage = json!({
            "input_tokens": 1_000_000, "output_tokens": 1_000_000,
            "cache_read_input_tokens": 1_000_000, "cache_creation_input_tokens": 3_000_000,
            "cache_creation": {"ephemeral_5m_input_tokens": 2_000_000, "ephemeral_1h_input_tokens": 1_000_000},
            "server_tool_use": {"web_search_requests": 3},
            "speed": "standard", "inference_geo": "global",
        });
        // 4 + 2 × 5 + 8 + 0.2 + 20, and three searches.
        assert_eq!(cost("claude-opus-5-5", &usage), Some(42.23));
        assert_eq!(cost("claude-haiku-4-5-20251001", &usage), Some(10.63));
        assert_eq!(cost("claude-opus-9", &usage), None);
    }

    #[test]
    fn a_write_with_no_breakdown_goes_to_the_five_minute_cache() {
        let usage = json!({"input_tokens": 0, "output_tokens": 0,
            "cache_creation_input_tokens": 1_000_000});
        assert_eq!(cost("claude-sonnet-5-5", &usage), Some(2.5));
    }

    #[test]
    fn fast_mode_us_inference_and_a_long_haiku_prompt_raise_the_price() {
        let usage = |extra: serde_json::Value| {
            let mut usage = json!({"input_tokens": 1_000_000, "output_tokens": 0});
            usage
                .as_object_mut()
                .expect("an object")
                .extend(extra.as_object().expect("an object").clone());
            usage
        };
        assert_eq!(
            cost("claude-opus-5-5", &usage(json!({"speed": "fast"}))),
            Some(8.0)
        );
        assert_eq!(
            cost("claude-opus-5-5", &usage(json!({"inference_geo": "us"}))),
            Some(4.4)
        );
        assert_eq!(cost("claude-haiku-5-5", &usage(json!({}))), Some(0.5));
        let short = json!({"input_tokens": 100_000, "output_tokens": 0});
        assert_eq!(cost("claude-haiku-5-5", &short), Some(0.01));
    }
}
