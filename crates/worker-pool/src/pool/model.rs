//! Choosing which of a provider's models works an item.

use agent_backend::ModelOption;
use serde::Deserialize;
use work_queue::{State as ItemState, WorkItem};

use super::run::Kind;
use super::{PoolConfig, Shared};

/// How much of the item, and of a rework's feedback, the choosing agent sees.
const EXCERPT_CHARS: usize = 8000;

/// The agent's answer: the number of the option it picked.
#[derive(Debug, Deserialize)]
struct Choice {
    choice: usize,
}

impl Shared {
    /// With several models configured for the run's provider, has an agent
    /// read the item (and a rework's feedback) and pick the one that suits
    /// it; `config` then runs that model alone. Any trouble choosing leaves
    /// the provider's default model: the choice is a nicety, not the work.
    pub(super) async fn choose_model(
        &self,
        item: &WorkItem,
        kind: &Kind,
        config: PoolConfig,
    ) -> PoolConfig {
        let options = config.provider.options();
        if options.len() < 2 || self.stopped(config.provider.provider) {
            return config;
        }
        let Ok(body) = self.queue.read(ItemState::InProgress, item) else {
            return config;
        };
        let feedback = match kind {
            Kind::Rework { feedback, .. } => Some(feedback.as_str()),
            Kind::New => None,
        };
        let prompt = model_prompt(item, &body, feedback, &options);
        let name = format!("{}-{}-model", item.project, item.stem());
        let picked = match self.ask_message(&config, &prompt, &name).await {
            Ok(message) => parse_choice(&message, options.len()),
            Err(_) => None,
        };
        match picked {
            Some(i) => PoolConfig {
                provider: config.provider.with_option(&options[i]),
                ..config
            },
            None => config,
        }
    }
}

fn excerpt(text: &str) -> &str {
    let text = text.trim();
    match text.char_indices().nth(EXCERPT_CHARS) {
        Some((end, _)) => &text[..end],
        None => text,
    }
}

fn model_prompt(
    item: &WorkItem,
    body: &str,
    feedback: Option<&str>,
    options: &[ModelOption],
) -> String {
    let mut prompt = format!(
        "Choose which AI model should {task}. Judge how hard the work is: its scope, how \
         many parts of the code it touches, how much judgment it needs. Prefer the \
         cheapest option that can do it well.\n\n\
         <work-item name=\"{item}\">\n{body}\n</work-item>\n\n",
        task = if feedback.is_some() {
            "rework this item's pull request"
        } else {
            "carry out this work item"
        },
        body = excerpt(body),
    );
    if let Some(feedback) = feedback {
        prompt.push_str(&format!(
            "<feedback>\n{}\n</feedback>\n\n",
            excerpt(feedback)
        ));
    }
    prompt.push_str("Options:\n");
    for (i, option) in options.iter().enumerate() {
        let note = option
            .description
            .as_deref()
            .map(|d| format!(": {d}"))
            .unwrap_or_else(|| {
                if i == 0 {
                    ": the default".into()
                } else {
                    String::new()
                }
            });
        prompt.push_str(&format!("{i}. {}{note}\n", option.label()));
    }
    prompt
        .push_str("\nAnswer with a single JSON object and nothing else: {\"choice\": <number>}\n");
    prompt
}

/// The option number in the agent's final message, if it names one that
/// exists.
fn parse_choice(message: &str, options: usize) -> Option<usize> {
    let start = message.find('{')?;
    let end = message.rfind('}')?;
    let choice: Choice = serde_json::from_str(message.get(start..=end)?).ok()?;
    (choice.choice < options).then_some(choice.choice)
}

#[cfg(test)]
mod tests {
    // Test code asserts rather than propagating; see Rust code review.
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic_in_result_fn)]

    use super::*;

    #[test]
    fn reads_a_choice_in_range_only() {
        assert_eq!(parse_choice("```json\n{\"choice\": 1}\n```", 2), Some(1));
        assert_eq!(parse_choice("{\"choice\": 2}", 2), None);
        assert_eq!(parse_choice("no idea", 2), None);
    }

    #[test]
    fn prompt_lists_every_option_and_the_feedback() {
        let options = vec![
            ModelOption {
                model: Some("sonnet".into()),
                effort: None,
                description: None,
            },
            ModelOption {
                model: Some("opus".into()),
                effort: Some("high".into()),
                description: Some("hard changes".into()),
            },
        ];
        let item = WorkItem::new("phase", "a.md");
        let prompt = model_prompt(&item, "do a thing", Some("fix the test"), &options);
        assert!(prompt.contains("0. sonnet: the default"));
        assert!(prompt.contains("1. opus, high effort: hard changes"));
        assert!(prompt.contains("fix the test"));
        assert!(prompt.contains("rework"));
    }
}
