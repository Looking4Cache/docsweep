//! The `docsweep` command line tool.

use clap::Parser;
use docsweep_core::names;

#[derive(Parser)]
#[command(name = names::LOWERCASE_NAME, version, about = "Collects incoming invoices")]
struct Cli {}

fn main() {
    Cli::parse();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }
}
