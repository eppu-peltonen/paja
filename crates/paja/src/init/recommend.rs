//! `paja init --recommend`: stacks for a project that doesn't exist yet.
//!
//! One agent on the `init` role, with no repository to read, is told what the person is building
//! and which languages or frameworks they prefer, and answers with one to three options, best
//! first. Paja decides which options have a built-in skeleton (`rust`, `node`, `python`); the rest
//! are `generated`. `paja init --new` without `--stack` asks the person on a terminal, shows the
//! options and takes a number or a stack name.

use std::io::{BufRead, Write};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::STACKS;
use crate::home::Home;

/// The most options shown.
const MAX_OPTIONS: usize = 3;

/// One option: a stack, why it fits, and what it costs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Recommendation {
    pub stack: String,
    pub why: String,
    pub tradeoffs: String,
    /// `built-in` for rust, node and python, `generated` otherwise.
    pub skeleton: String,
}

/// What `paja init --recommend` answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Recommendations {
    /// One to three options, best first.
    pub recommendations: Vec<Recommendation>,
    /// What the agent would still like to know, for the person to answer and ask again.
    pub questions: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Raw {
    recommendations: Vec<RawOption>,
    #[serde(default)]
    questions: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawOption {
    stack: String,
    #[serde(default)]
    why: String,
    #[serde(default)]
    tradeoffs: String,
}

/// The skeleton a stack gets: Paja's own table decides, never the agent.
pub fn skeleton_of(stack: &str) -> &'static str {
    if STACKS.contains(&stack.trim().to_ascii_lowercase().as_str()) {
        "built-in"
    } else {
        "generated"
    }
}

/// The JSON Schema the agent's answer must follow.
pub fn schema() -> Value {
    let text = |what: &str| json!({"type": "string", "description": what});
    json!({
        "type": "object",
        "properties": {
            "recommendations": {
                "type": "array",
                "minItems": 1,
                "maxItems": MAX_OPTIONS,
                "items": {
                    "type": "object",
                    "properties": {
                        "stack": text("The language or framework; exactly `rust`, `node` or `python` for those"),
                        "why": text("Why it fits what is being built, in a sentence or two"),
                        "tradeoffs": text("What it costs, in a sentence or two")
                    },
                    "required": ["stack", "why", "tradeoffs"]
                }
            },
            "questions": {
                "type": "array",
                "items": {"type": "string"},
                "description": "What you would still like to know before choosing"
            }
        },
        "required": ["recommendations", "questions"]
    })
}

/// The prompt: what is being built and what the person prefers. Both are the person's own words.
pub fn prompt(building: &str, prefer: &[String]) -> String {
    let mut text = format!(
        "You recommend the technology stack for a new software project. You have no repository \
         to read and need none: answer from what is below.\n\n\
         What is being built:\n{}\n\n",
        building.trim()
    );
    if prefer.is_empty() {
        text.push_str("The person stated no preferred language or framework.\n\n");
    } else {
        text.push_str(&format!("The person prefers: {}.\n\n", prefer.join(", ")));
    }
    text.push_str(
        "Answer with one to three options, best first. Stated preferences win: put them first and \
         recommend them unless one is a poor fit for what is being built, and then say so in that \
         option's `why`. Name `rust`, `node` and `python` exactly so; Paja has a ready-made \
         skeleton for those, and generates one for any other stack. Pick stacks with a formatter, \
         a linter and a test runner, since the project's checks are built from them. Keep `why` \
         and `tradeoffs` to a sentence or two each. Put in `questions` only what would change your \
         answer; leave it empty when nothing would.\n",
    );
    text
}

/// Read the agent's `structured_output`; anything else than the documented shape is an error.
pub fn parse(value: Value) -> Result<Recommendations> {
    let raw: Raw = serde_json::from_value(value).context(
        "the init agent's answer is not the documented JSON (an object with a `recommendations` list)",
    )?;
    let recommendations: Vec<Recommendation> = raw
        .recommendations
        .into_iter()
        .filter(|o| !o.stack.trim().is_empty())
        .take(MAX_OPTIONS)
        .map(|o| {
            let stack = o.stack.trim().to_string();
            Recommendation {
                skeleton: skeleton_of(&stack).to_string(),
                stack,
                why: o.why.trim().to_string(),
                tradeoffs: o.tradeoffs.trim().to_string(),
            }
        })
        .collect();
    if recommendations.is_empty() {
        bail!("the init agent recommended no stack; run `paja init --recommend` again");
    }
    Ok(Recommendations {
        recommendations,
        questions: raw
            .questions
            .into_iter()
            .map(|q| q.trim().to_string())
            .filter(|q| !q.is_empty())
            .collect(),
    })
}

/// Ask the init agent which stacks fit `building`, honouring the stated `prefer`ences.
pub fn recommend(home: &Home, building: &str, prefer: &[String]) -> Result<Recommendations> {
    if building.trim().is_empty() {
        bail!(
            "say what you are building, for example `paja init --recommend \"a web app that \
             tracks my reading list\"`"
        );
    }
    crate::mestari::progress("init: asking the init agent which stacks fit");
    let answer = crate::mestari::recommend_agent(home, &prompt(building, prefer), &schema())?;
    parse(answer)
}

/// The line `--prefer` text becomes: split on commas, blanks dropped.
fn split_preferences(line: &str) -> Vec<String> {
    line.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

fn ask(input: &mut dyn BufRead, out: &mut dyn Write, question: &str) -> Result<String> {
    write!(out, "{question}")?;
    out.flush()?;
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        bail!(
            "the input ended before every question was answered; pass --stack rust, node or python, \
             or run `paja init --recommend \"<what you are building>\"` first"
        );
    }
    Ok(line.trim().to_string())
}

/// The stack named by `answer` among `options`: a 1-based number, or a stack name.
fn pick(answer: &str, options: &[Recommendation]) -> Result<String> {
    if let Ok(n) = answer.parse::<usize>() {
        return match n.checked_sub(1).and_then(|i| options.get(i)) {
            Some(option) => Ok(option.stack.clone()),
            None => bail!(
                "{n} is not one of the options; run again and type a number from 1 to {}, or a stack name",
                options.len()
            ),
        };
    }
    if answer.is_empty() {
        bail!(
            "no stack chosen; run again and type a number from 1 to {}, or a stack name",
            options.len()
        );
    }
    Ok(options
        .iter()
        .find(|o| o.stack.eq_ignore_ascii_case(answer))
        .map_or_else(|| answer.to_string(), |o| o.stack.clone()))
}

/// The prompt flow of `paja init --new` without `--stack`: ask what is being built and the
/// preferences on `input`, get options from `ask_agent`, show them on `out`, and take a number or a
/// stack name. Returns the built-in stack to scaffold; a `generated` skeleton stops with an error
/// until Paja can write one.
pub fn choose_stack(
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    ask_agent: &mut dyn FnMut(&str, &[String]) -> Result<Recommendations>,
) -> Result<String> {
    let building = ask(input, out, "What are you building? ")?;
    if building.is_empty() {
        bail!("say what you are building; run again, or pass --stack rust, node or python");
    }
    let prefer = split_preferences(&ask(
        input,
        out,
        "Preferred language or framework (comma-separated, Enter for none): ",
    )?);
    let found = ask_agent(&building, &prefer)?;
    writeln!(out, "\nRecommended, best first:")?;
    for (i, o) in found.recommendations.iter().enumerate() {
        writeln!(out, "  {}. {} ({} skeleton)", i + 1, o.stack, o.skeleton)?;
        writeln!(out, "     Why: {}", o.why)?;
        writeln!(out, "     Trade-offs: {}", o.tradeoffs)?;
    }
    for q in &found.questions {
        writeln!(out, "  ? {q}")?;
    }
    let answer = ask(input, out, "\nPick a number or type a stack name: ")?;
    let stack = pick(&answer, &found.recommendations)?;
    if skeleton_of(&stack) == "generated" {
        bail!(
            "{stack} needs a generated skeleton, which Paja can't write yet; pick a built-in stack \
             ({}), for example `paja init --new NAME --stack rust`",
            STACKS.join(", ")
        );
    }
    Ok(stack.trim().to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(stack: &str) -> Value {
        json!({"stack": stack, "why": "fits", "tradeoffs": "costs"})
    }

    #[test]
    fn skeletons_are_decided_by_paja_and_options_are_capped_at_three() {
        let found = parse(json!({
            "recommendations": [option("rust"), option("Go"), option("python"), option("node")],
            "questions": ["Who uses it?", " "]
        }))
        .unwrap();
        let shape: Vec<(&str, &str)> = found
            .recommendations
            .iter()
            .map(|o| (o.stack.as_str(), o.skeleton.as_str()))
            .collect();
        assert_eq!(
            shape,
            [
                ("rust", "built-in"),
                ("Go", "generated"),
                ("python", "built-in")
            ]
        );
        assert_eq!(found.questions, ["Who uses it?"]);
        assert!(parse(json!({"recommendations": []})).is_err());
        assert!(parse(json!({"stacks": []})).is_err());
    }

    fn fake(found: &'static str) -> impl FnMut(&str, &[String]) -> Result<Recommendations> {
        move |building, prefer| {
            assert_eq!(building, "a reading list");
            assert_eq!(prefer, ["rust", "axum"]);
            parse(json!({"recommendations": [option(found), option("go")], "questions": []}))
        }
    }

    fn run(script: &str, found: &'static str) -> (Result<String>, String) {
        let mut out = Vec::new();
        let result = choose_stack(&mut script.as_bytes(), &mut out, &mut fake(found));
        (result, String::from_utf8(out).unwrap())
    }

    #[test]
    fn a_number_or_a_stack_name_picks_the_stack() {
        let (result, shown) = run("a reading list\nrust, axum\n1\n", "rust");
        assert_eq!(result.unwrap(), "rust");
        assert!(shown.contains("1. rust (built-in skeleton)"), "{shown}");
        assert!(shown.contains("2. go (generated skeleton)"), "{shown}");
        let (result, _) = run("a reading list\nrust, axum\n  PYTHON \n", "rust");
        assert_eq!(result.unwrap(), "python");
    }

    #[test]
    fn a_generated_skeleton_or_a_bad_answer_stops_with_the_fix() {
        let (result, _) = run("a reading list\nrust, axum\n2\n", "rust");
        let error = format!("{:#}", result.unwrap_err());
        assert!(error.contains("go needs a generated skeleton"), "{error}");
        assert!(error.contains("rust, node, python"), "{error}");
        let (result, _) = run("a reading list\nrust, axum\n9\n", "rust");
        assert!(format!("{:#}", result.unwrap_err()).contains("1 to 2"));
        let (result, _) = run("a reading list\n", "rust");
        assert!(format!("{:#}", result.unwrap_err()).contains("--stack"));
    }
}
