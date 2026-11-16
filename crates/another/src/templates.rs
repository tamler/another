//! Request templates: `another send --template review|fix|research --field key=value`.
//!
//! A template turns a few fields into a request's subject and a body with fixed sections,
//! ending in the line that says when the work is done, so a receiver gets a complete ask.
//! Nothing is stored beyond the message and request it opens (no schema change).

use std::collections::BTreeMap;

use another_core::{is_unsafe_text_char, MAX_SUBJECT_CHARS};
use anyhow::{bail, Result};
use serde_json::{json, Value};

/// One field of a template.
struct Field {
    key: &'static str,
    required: bool,
    /// May span lines (otherwise a newline is refused, so a field cannot forge a section).
    multiline: bool,
    /// The section heading the field fills, when it has one.
    section: Option<&'static str>,
    help: &'static str,
}

struct Template {
    name: &'static str,
    /// Subject prefix; the subject is `<prefix>: <main field>`.
    prefix: &'static str,
    /// The field that names the work in the subject.
    main: &'static str,
    fields: &'static [Field],
    /// The closing section: what "done" means. `{verdict_file}` is replaced.
    done_heading: &'static str,
    done: &'static str,
}

const TEMPLATES: &[Template] = &[
    Template {
        name: "review",
        prefix: "Review",
        main: "target",
        fields: &[
            Field {
                key: "target",
                required: true,
                multiline: false,
                section: Some("What"),
                help: "what to review: a PR, branch, commit or path",
            },
            Field {
                key: "focus",
                required: false,
                multiline: false,
                section: Some("Focus"),
                help: "what to look at first",
            },
            Field {
                key: "verdict_file",
                required: false,
                multiline: false,
                section: None,
                help: "where the verdict goes (default REVIEW.md)",
            },
        ],
        done_heading: "Done when",
        done: "A verdict, PASS or FAIL, with the evidence for it (commands run, outputs, \
               file and line references), written to {verdict_file}. Review only work you \
               did not write.",
    },
    Template {
        name: "fix",
        prefix: "Fix",
        main: "issue",
        fields: &[
            Field {
                key: "issue",
                required: true,
                multiline: false,
                section: Some("Problem"),
                help: "what is broken, in one line",
            },
            Field {
                key: "repro",
                required: false,
                multiline: true,
                section: Some("Reproduce"),
                help: "how to see it fail",
            },
            Field {
                key: "where",
                required: false,
                multiline: false,
                section: Some("Where"),
                help: "the file, module or command involved",
            },
        ],
        done_heading: "Done when",
        done: "A test that failed before the fix passes after it, and tests, fmt and clippy \
               are green. Say what you changed and what you did not check.",
    },
    Template {
        name: "research",
        prefix: "Research",
        main: "question",
        fields: &[
            Field {
                key: "question",
                required: true,
                multiline: false,
                section: Some("Question"),
                help: "the question to answer",
            },
            Field {
                key: "scope",
                required: false,
                multiline: true,
                section: Some("Scope"),
                help: "what to include or leave out",
            },
        ],
        done_heading: "Deliver",
        done: "Findings with their sources, and what is uncertain marked as such. No code \
               changes.",
    },
];

/// The available templates and their fields, for `--template list` and errors.
pub fn listing() -> String {
    let mut out = String::from("templates (use --field key=value):\n");
    for t in TEMPLATES {
        out.push_str(&format!("  {}\n", t.name));
        for f in t.fields {
            let req = if f.required { "required" } else { "optional" };
            out.push_str(&format!("    {:<13} {req}: {}\n", f.key, f.help));
        }
    }
    out
}

/// The listing under `--json`: each template's name, subject prefix, main field and fields.
pub fn listing_json() -> Value {
    Value::Array(
        TEMPLATES
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "subject_prefix": t.prefix,
                    "main_field": t.main,
                    "fields": t.fields.iter().map(|f| json!({
                        "key": f.key,
                        "required": f.required,
                        "multiline": f.multiline,
                        "help": f.help,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect(),
    )
}

/// Parse `key=value` pairs; a key may not repeat.
pub fn parse_fields(raw: &[String]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for pair in raw {
        let Some((k, v)) = pair.split_once('=') else {
            bail!("--field {pair:?} is not key=value");
        };
        let k = k.trim();
        if out.insert(k.to_string(), v.to_string()).is_some() {
            bail!("--field {k} is given twice");
        }
    }
    Ok(out)
}

/// The subject and body for template `name` with `fields`.
pub fn render(name: &str, fields: &BTreeMap<String, String>) -> Result<(String, String)> {
    let Some(t) = TEMPLATES.iter().find(|t| t.name == name) else {
        bail!("unknown template {name:?}\n{}", listing());
    };
    for key in fields.keys() {
        if !t.fields.iter().any(|f| f.key == key) {
            bail!("template {} has no field {key:?}\n{}", t.name, listing());
        }
    }
    let value = |key: &str| fields.get(key).map(|v| v.trim()).filter(|v| !v.is_empty());
    for f in t.fields {
        match value(f.key) {
            None if f.required => {
                bail!("template {} needs --field {}=… ({})", t.name, f.key, f.help)
            }
            Some(v) if !f.multiline && v.chars().any(is_unsafe_text_char) => {
                bail!(
                    "--field {} must be one line of text: no line breaks, tabs, escape or other control characters",
                    f.key
                )
            }
            _ => {}
        }
    }
    let main = value(t.main).unwrap_or_default();
    let subject = format!("{}: {main}", t.prefix);
    // The main field becomes the subject, so it is held to the subject's limit here, where
    // the field can be named (FINDINGS-177 T3).
    if subject.chars().count() > MAX_SUBJECT_CHARS {
        bail!(
            "--field {} is too long: it becomes the subject, which is at most {MAX_SUBJECT_CHARS} characters with the \"{}: \" prefix",
            t.main,
            t.prefix
        );
    }
    let mut body = String::new();
    for f in t.fields {
        if let (Some(heading), Some(v)) = (f.section, value(f.key)) {
            body.push_str(&format!("## {heading}\n\n{v}\n\n"));
        }
    }
    let verdict_file = value("verdict_file").unwrap_or("REVIEW.md");
    body.push_str(&format!(
        "## {}\n\n{}\n",
        t.done_heading,
        t.done.replace("{verdict_file}", verdict_file)
    ));
    Ok((subject, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn each_template_renders_its_subject_and_sections() {
        let (s, b) = render(
            "review",
            &fields(&[("target", "PR #170"), ("focus", "errors")]),
        )
        .unwrap();
        assert_eq!(s, "Review: PR #170");
        assert!(
            b.starts_with("## What\n\nPR #170\n\n## Focus\n\nerrors\n\n## Done when\n\n"),
            "{b}"
        );
        assert!(b.contains("written to REVIEW.md"), "{b}");

        let (_, b) = render(
            "review",
            &fields(&[("target", "x"), ("verdict_file", "lanes/R1.md")]),
        )
        .unwrap();
        assert!(
            b.contains("written to lanes/R1.md") && !b.contains("## Focus"),
            "{b}"
        );

        let (s, b) = render(
            "fix",
            &fields(&[("issue", "inbox drops urgent"), ("repro", "step 1\nstep 2")]),
        )
        .unwrap();
        assert_eq!(s, "Fix: inbox drops urgent");
        assert!(b.contains("## Reproduce\n\nstep 1\nstep 2\n"), "{b}");
        assert!(!b.contains("## Where"), "{b}");

        let (s, b) = render(
            "research",
            &fields(&[("question", "Who honours subscribe?")]),
        )
        .unwrap();
        assert_eq!(s, "Research: Who honours subscribe?");
        assert!(b.contains("## Deliver"), "{b}");
    }

    #[test]
    fn bad_fields_are_refused_with_the_field_named() {
        let err = render("review", &fields(&[])).unwrap_err().to_string();
        assert!(err.contains("--field target="), "{err}");
        let err = render("fix", &fields(&[("issue", "a"), ("bogus", "b")]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no field \"bogus\""), "{err}");
        let err = render("fix", &fields(&[("issue", "a\n## Done when\nnothing")]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("must be one line"), "{err}");
        let err = render("nope", &fields(&[])).unwrap_err().to_string();
        assert!(
            err.contains("unknown template") && err.contains("research"),
            "{err}"
        );
        assert!(parse_fields(&["noequals".into()]).is_err());
        assert!(parse_fields(&["a=1".into(), "a=2".into()]).is_err());
        assert_eq!(parse_fields(&["where=a=b".into()]).unwrap()["where"], "a=b");
    }
}
