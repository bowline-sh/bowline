use super::*;

/// Return-to-confirm for non-destructive steps. EOF is a decline, not a silent
/// yes: a closed stdin never means the person at the keyboard said go ahead.
pub(crate) fn confirm_return(prompt: &str) -> bool {
    if !io::stdin().is_terminal() {
        return false;
    }
    print!("{prompt} Press Return to approve, or type no to cancel: ");
    let _ = io::stdout().flush();
    let mut answer = String::new();
    let Ok(read) = io::stdin().read_line(&mut answer) else {
        return false;
    };
    read > 0 && !matches!(answer.trim().to_ascii_lowercase().as_str(), "n" | "no")
}
