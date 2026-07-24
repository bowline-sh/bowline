use std::collections::BTreeMap;

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedValues {
    options: BTreeMap<&'static str, Vec<String>>,
    positionals: Vec<String>,
}

impl ParsedValues {
    pub(crate) fn flag(&self, name: &str) -> bool {
        self.options.contains_key(name)
    }

    pub(crate) fn option(&self, name: &str) -> Option<&str> {
        self.options
            .get(name)
            .and_then(|values| values.first())
            .map(String::as_str)
    }

    pub(crate) fn options(&self, name: &str) -> impl Iterator<Item = &str> {
        self.options
            .get(name)
            .into_iter()
            .flatten()
            .map(String::as_str)
    }

    pub(crate) fn positionals(&self) -> &[String] {
        &self.positionals
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DefinitionInvocation {
    pub(crate) json: bool,
    pub(crate) human: bool,
    pub(crate) quiet: bool,
    pub(crate) socket: PathBuf,
    pub(crate) dry_run: bool,
    pub(crate) values: ParsedValues,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedDefinition {
    pub(crate) target: DefinitionTarget,
    pub(crate) invocation: DefinitionInvocation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DefinitionTarget {
    Public(CommandName),
    DebugClassify,
    SyncWait,
    SyncAttention,
    SyncRetry,
    SyncDismiss,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DefinitionFailure {
    pub(crate) json: bool,
    pub(crate) human: bool,
    pub(crate) quiet: bool,
    pub(crate) error: ParseError,
}

pub(crate) fn resolve_definition(args: &[String]) -> Result<ResolvedDefinition, DefinitionFailure> {
    let default_command = ["help".to_string()];
    let normalized_args = normalize_discovery_aliases(args);
    let effective_args: &[String] = if normalized_args.is_empty() {
        default_command.as_slice()
    } else {
        normalized_args.as_slice()
    };
    let Some((spec, path_len)) = all_command_specs()
        .filter_map(|spec| {
            command_path_matches(effective_args, spec.name)
                .then_some((spec, spec.name.split_whitespace().count()))
        })
        .max_by_key(|(_, path_len)| *path_len)
    else {
        return Err(DefinitionFailure {
            json: false,
            human: false,
            quiet: false,
            error: ParseError::Unknown(
                effective_args
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "help".to_string()),
            ),
        });
    };
    let tail = &effective_args[path_len..];
    let (json, human, quiet) = output_hints(spec, tail);
    parse_definition_arguments(spec, tail)
        .map(|invocation| ResolvedDefinition {
            target: spec.target(),
            invocation,
        })
        .map_err(|error| DefinitionFailure {
            json,
            human,
            quiet,
            error,
        })
}

fn normalize_discovery_aliases(args: &[String]) -> Vec<String> {
    if matches!(args.first().map(String::as_str), Some("--version" | "-V")) {
        return discovery_invocation("version", std::iter::empty(), args);
    }

    let Some(help_index) = args
        .iter()
        .take_while(|argument| argument.as_str() != "--")
        .position(|argument| matches!(argument.as_str(), "--help" | "-h"))
    else {
        return args.to_vec();
    };
    let prefix = &args[..help_index];
    let known_topic = all_command_specs()
        .filter_map(|spec| {
            command_path_matches(prefix, spec.name)
                .then_some((spec.name, spec.name.split_whitespace().count()))
        })
        .max_by_key(|(_, path_len)| *path_len)
        .map(|(name, _)| {
            name.split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>()
        });
    let topic: Vec<String> = known_topic.unwrap_or_else(|| {
        prefix
            .iter()
            .take_while(|argument| !argument.starts_with('-'))
            .cloned()
            .collect()
    });
    discovery_invocation("help", topic, args)
}

fn discovery_invocation(
    command: &str,
    topic: impl IntoIterator<Item = String>,
    args: &[String],
) -> Vec<String> {
    let mut normalized = vec![command.to_string()];
    normalized.extend(topic);
    normalized.extend(
        args.iter()
            .take_while(|argument| argument.as_str() != "--")
            .filter(|argument| matches!(argument.as_str(), "--json" | "--human"))
            .cloned(),
    );
    normalized
}

pub(super) fn parse_definition_arguments(
    spec: &'static CommandSpec,
    args: &[String],
) -> Result<DefinitionInvocation, ParseError> {
    let command = spec.command_name();
    let mut options = BTreeMap::<&'static str, Vec<String>>::new();
    let mut positionals = Vec::new();
    let mut positional_only = false;
    let mut index = 0;

    while index < args.len() {
        let argument = &args[index];
        if positional_only {
            positionals.push(argument.clone());
            index += 1;
            continue;
        }
        if argument == "--" {
            positional_only = true;
            index += 1;
            continue;
        }
        let Some((spelling, inline_value)) = split_option(argument) else {
            positionals.push(argument.clone());
            index += 1;
            continue;
        };
        let Some(option) = definition_option(spec, spelling) else {
            return Err(ParseError::Usage {
                command,
                message: format!("unknown bowline {} option `{spelling}`", spec.name),
            });
        };
        if options.contains_key(option.name) && !option.repeatable {
            return Err(ParseError::Usage {
                command,
                message: format!(
                    "bowline {} option `{}` cannot be repeated",
                    spec.name, option.name
                ),
            });
        }

        let value = match (option.value_name, inline_value) {
            (None, None) => String::new(),
            (None, Some(_)) => {
                return Err(ParseError::Usage {
                    command,
                    message: format!(
                        "bowline {} flag `{}` takes no value",
                        spec.name, option.name
                    ),
                });
            }
            (Some(_), Some(value)) if !value.is_empty() => value.to_string(),
            (Some(_), Some(_)) => {
                return Err(missing_option_value(spec, option.name));
            }
            (Some(_), None) => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| missing_option_value(spec, option.name))?;
                if value == "--"
                    || split_option(value)
                        .and_then(|(spelling, _)| definition_option(spec, spelling))
                        .is_some()
                {
                    return Err(missing_option_value(spec, option.name));
                }
                value.clone()
            }
        };
        options.entry(option.name).or_default().push(value);
        index += 1;
    }

    for option in spec.options {
        if option.required && !options.contains_key(option.name) {
            return Err(missing_option_value(spec, option.name));
        }
    }
    validate_positionals(spec, &positionals)?;

    let json = options.remove("--json").is_some();
    let human = options.remove("--human").is_some();
    let quiet = options.remove("--quiet").is_some();
    let dry_run = options.remove("--dry-run").is_some();
    let socket = options
        .remove("--socket")
        .and_then(|values| values.into_iter().next())
        .map(PathBuf::from)
        .unwrap_or_else(default_socket_path);

    if json && human {
        return Err(ParseError::Usage {
            command,
            message: "--json and --human cannot be used together".to_string(),
        });
    }
    if quiet && (json || human) {
        return Err(ParseError::Usage {
            command,
            message: "--quiet cannot be combined with --json or --human".to_string(),
        });
    }

    Ok(DefinitionInvocation {
        json,
        human,
        quiet,
        socket,
        dry_run,
        values: ParsedValues {
            options,
            positionals,
        },
    })
}

fn validate_positionals(spec: &CommandSpec, positionals: &[String]) -> Result<(), ParseError> {
    let minimum = spec
        .positionals
        .iter()
        .filter(|positional| positional.required)
        .count();
    let maximum = (!spec
        .positionals
        .last()
        .is_some_and(|positional| positional.repeatable))
    .then_some(spec.positionals.len());
    if positionals.len() < minimum {
        let missing = spec
            .positionals
            .iter()
            .skip(positionals.len())
            .find(|positional| positional.required)
            .map(|positional| positional.name)
            .unwrap_or("argument");
        return Err(ParseError::Usage {
            command: spec.command_name(),
            message: format!("bowline {} requires <{missing}>", spec.name),
        });
    }
    if maximum.is_some_and(|maximum| positionals.len() > maximum) {
        return Err(ParseError::Usage {
            command: spec.command_name(),
            message: format!(
                "unexpected bowline {} argument `{}`",
                spec.name,
                positionals[spec.positionals.len()]
            ),
        });
    }
    Ok(())
}

fn split_option(argument: &str) -> Option<(&str, Option<&str>)> {
    if !argument.starts_with('-') || argument == "-" {
        return None;
    }
    Some(match argument.split_once('=') {
        Some((spelling, value)) => (spelling, Some(value)),
        None => (argument, None),
    })
}

fn command_path_matches(args: &[String], name: &str) -> bool {
    let tokens = name.split_whitespace().collect::<Vec<_>>();
    args.len() >= tokens.len()
        && tokens
            .iter()
            .zip(args.iter())
            .all(|(expected, actual)| *expected == actual)
}

fn output_hints(spec: &CommandSpec, args: &[String]) -> (bool, bool, bool) {
    let mut json = false;
    let mut human = false;
    let mut quiet = false;
    for argument in args {
        if argument == "--" {
            break;
        }
        match argument.as_str() {
            "--json" if spec.supports_json => json = true,
            "--human" if spec.supports_json => human = true,
            "--quiet" if spec.options.iter().any(|option| option.name == "--quiet") => quiet = true,
            _ => {}
        }
    }
    (json, human, quiet)
}

fn definition_option(spec: &'static CommandSpec, spelling: &str) -> Option<&'static OptionSpec> {
    spec.options
        .iter()
        .find(|option| option.name == spelling)
        .or_else(|| (spelling == "--human" && spec.supports_json).then_some(&HUMAN_OPTION))
}

fn missing_option_value(spec: &CommandSpec, option: &str) -> ParseError {
    ParseError::Usage {
        command: spec.command_name(),
        message: format!("bowline {} {option} requires a value", spec.name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_terminator_preserves_leading_dash_positionals() {
        let spec = command_specs()
            .find(|spec| spec.name == "help")
            .expect("help definition");
        let invocation =
            parse_definition_arguments(spec, &["--".to_string(), "--not-an-option".to_string()])
                .expect("parsed arguments");

        assert_eq!(invocation.values.positionals(), &["--not-an-option"]);
    }

    #[test]
    fn rejects_repeated_single_value_options() {
        let spec = command_specs()
            .find(|spec| spec.name == "status")
            .expect("status definition");
        let error = parse_definition_arguments(
            spec,
            &[
                "--root".to_string(),
                "/one".to_string(),
                "--root=/two".to_string(),
            ],
        )
        .expect_err("repeated root must fail");

        assert!(matches!(error, ParseError::Usage { .. }));
    }

    #[test]
    fn conventional_discovery_aliases_resolve_to_canonical_commands() {
        assert_eq!(
            normalize_discovery_aliases(&["--help".to_string(), "--json".to_string()]),
            ["help", "--json"]
        );
        assert_eq!(
            normalize_discovery_aliases(&[
                "work".to_string(),
                "create".to_string(),
                "ignored-project".to_string(),
                "-h".to_string(),
                "--human".to_string(),
            ]),
            ["help", "work", "create", "--human"]
        );
        assert_eq!(
            normalize_discovery_aliases(&["sync".to_string(), "--help".to_string()]),
            ["help", "sync"]
        );
        assert_eq!(
            normalize_discovery_aliases(&["-V".to_string(), "--json".to_string()]),
            ["version", "--json"]
        );
        assert_eq!(
            normalize_discovery_aliases(&[
                "help".to_string(),
                "--".to_string(),
                "--help".to_string(),
                "--json".to_string(),
            ]),
            ["help", "--", "--help", "--json"]
        );
    }
}
