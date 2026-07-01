use super::{ResolveAgent, parse_agent};

#[test]
fn parses_known_agents_only() {
    assert_eq!(parse_agent("codex"), Some(ResolveAgent::Codex));
    assert_eq!(parse_agent("claude"), Some(ResolveAgent::Claude));
    assert_eq!(parse_agent("cursor"), Some(ResolveAgent::Cursor));
    assert_eq!(parse_agent("git"), None);
}
