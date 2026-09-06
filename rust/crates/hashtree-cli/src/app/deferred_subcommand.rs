use clap::{ArgMatches, Command, Error, FromArgMatches, Subcommand};

/// Build a large nested command group only when Clap enters that group. This
/// keeps unrelated generated builders from sharing the startup call stack.
pub(crate) struct DeferredSubcommand<T>(pub(crate) T);

impl<T: FromArgMatches> FromArgMatches for DeferredSubcommand<T> {
    fn from_arg_matches(matches: &ArgMatches) -> Result<Self, Error> {
        T::from_arg_matches(matches).map(Self)
    }

    fn from_arg_matches_mut(matches: &mut ArgMatches) -> Result<Self, Error> {
        T::from_arg_matches_mut(matches).map(Self)
    }

    fn update_from_arg_matches(&mut self, matches: &ArgMatches) -> Result<(), Error> {
        self.0.update_from_arg_matches(matches)
    }

    fn update_from_arg_matches_mut(&mut self, matches: &mut ArgMatches) -> Result<(), Error> {
        self.0.update_from_arg_matches_mut(matches)
    }
}

impl<T: Subcommand> Subcommand for DeferredSubcommand<T> {
    fn augment_subcommands(cmd: Command) -> Command {
        cmd.defer(T::augment_subcommands)
    }

    fn augment_subcommands_for_update(cmd: Command) -> Command {
        cmd.defer(T::augment_subcommands_for_update)
    }

    fn has_subcommand(name: &str) -> bool {
        T::has_subcommand(name)
    }
}
