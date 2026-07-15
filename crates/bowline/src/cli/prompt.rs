use super::*;

pub(crate) fn confirm_return(prompt: &str) -> bool {
    if !io::stdin().is_terminal() {
        return false;
    }
    print!("{prompt} Press Return to approve, or type no to cancel: ");
    let _ = io::stdout().flush();
    let mut answer = String::new();
    io::stdin().read_line(&mut answer).is_ok()
        && !matches!(answer.trim().to_ascii_lowercase().as_str(), "n" | "no")
}
