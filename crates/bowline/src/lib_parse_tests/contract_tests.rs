use super::{CommandName, ParseError, parse_args};

#[test]
fn summary_rejects_topics_in_either_option_order() {
    for args in [
        ["contract", "--summary", "status"],
        ["contract", "status", "--summary"],
    ] {
        let invocation = parse_args(args);
        assert!(matches!(
            invocation.command,
            Err(ParseError::Usage {
                command: CommandName::Contract,
                ..
            })
        ));
    }
}
