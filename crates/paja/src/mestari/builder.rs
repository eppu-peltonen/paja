use super::*;

/// How the builder asks the person for a decision: a line of its final message starting with
/// this, followed by the question.
pub(super) const ASK: &str = "PAJA-QUESTION:";

/// The question the builder asked in its final message, if it asked one.
pub(super) fn builder_question(out: &Value) -> Option<String> {
    let text = out["result"].as_str()?;
    let start = text
        .match_indices(ASK)
        .find(|(at, _)| *at == 0 || text[..*at].ends_with('\n'))?
        .0;
    let question = text[start + ASK.len()..].trim();
    (!question.is_empty()).then(|| question.to_string())
}

/// What a person answered to Paja's earlier questions on the item, for the agents' prompts.
/// Agents can't read the issue's comments, so this is how an answer reaches the next run.
pub(super) fn answers(item: &WorkItem) -> String {
    if item.answers.is_empty() {
        return String::new();
    }
    let mut text = String::from(
        "\nA person answered Paja's earlier questions about this issue. The answers settle what \
         the issue leaves open; follow them:\n",
    );
    for answer in &item.answers {
        text.push('\n');
        text.push_str(&show_answer(answer));
    }
    text
}

/// One answer with the question it settles, as the prompts show it.
pub(super) fn show_answer(answer: &jono::Answer) -> String {
    let mut text = String::new();
    if let Some(question) = &answer.question {
        text.push_str(&format!("Question: {question}\n"));
    }
    text.push_str(&format!("Answer: {}\n", answer.answer));
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builder_asks_only_on_a_line_of_its_own() {
        let ask = |text: &str| builder_question(&json!({ "result": text }));
        assert_eq!(
            ask("Stuck.\nPAJA-QUESTION: A or B? I'd pick A.\nThanks").as_deref(),
            Some("A or B? I'd pick A.\nThanks")
        );
        assert_eq!(ask("PAJA-QUESTION: Which?").as_deref(), Some("Which?"));
        assert_eq!(ask("Done. No PAJA-QUESTION: here."), None);
        assert_eq!(ask("PAJA-QUESTION:   \n"), None);
        assert_eq!(builder_question(&json!({})), None);
    }
}
