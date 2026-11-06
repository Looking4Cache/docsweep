//! Asks a person in the terminal while a browser waits for them.

use docsweep_core::portal::driver::{
    ConfirmationAnswer, ConfirmationHandler, ConfirmationReason, ConfirmationRequest,
};
use tokio::sync::oneshot;

/// Waits for Enter. Without a terminal, standard input ends at once, which
/// counts as cancelling.
pub struct TerminalConfirmation;

impl ConfirmationHandler for TerminalConfirmation {
    async fn ask(&mut self, request: &ConfirmationRequest) -> ConfirmationAnswer {
        let what = match &request.reason {
            ConfirmationReason::Detected => {
                "the portal asks for a confirmation or a captcha".to_owned()
            }
            ConfirmationReason::StepTimedOut { step } => format!("this took too long: {step}"),
        };
        eprintln!(
            "\n{}: {what}.\nPlease finish this in the browser window, then press Enter here, or type c and Enter to cancel. Waiting for {} seconds.",
            request.company_name, request.timeout_secs
        );

        // Reading from the terminal cannot be interrupted. This thread simply
        // ends with the process if the time limit runs out first.
        let (sender, receiver) = oneshot::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let answer = match std::io::stdin().read_line(&mut line) {
                Ok(0) | Err(_) => ConfirmationAnswer::Cancelled,
                Ok(_) if line.trim().eq_ignore_ascii_case("c") => ConfirmationAnswer::Cancelled,
                Ok(_) => ConfirmationAnswer::Done,
            };
            let _ = sender.send(answer);
        });
        receiver.await.unwrap_or(ConfirmationAnswer::Cancelled)
    }

    fn expired(&mut self) {
        eprintln!("The time for this confirmation has run out.");
    }
}
