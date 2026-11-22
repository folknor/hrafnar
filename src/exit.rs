//! The exit statuses `hrafnar` itself ends with: the command's contract with
//! the scripts and agents that launch it (see `hrafnar --help`). One type, so
//! the codes are stated once and a whole command's status can be asserted on.

/// How a `hrafnar` command ended. `app::run` returns it and `main` exits with
/// it; an `Err` from `app::run` is reported and exits `Failed`, as it always
/// has. The signal supervisor is the one other exit site: it ends the process
/// from its own task once every turn it could end has been recorded, because
/// routing it through `main` would mean dropping the run's future, running
/// the in-flight leases' and keepers' destructors in the middle of a teardown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandExit {
    Success,
    /// The run failed: no turn, no answer, or an answer `hrafnar`
    /// could not verify. Also any error before or instead of a run.
    Failed,
    /// Asked to do nothing it could do: a bare `hrafnar` with nothing on stdin
    /// gets the help text, as a usage error does.
    Usage,
    /// Something a run started was left running, or its teardown could not be
    /// confirmed. Outranks `Failed`: processes that may still be changing the
    /// tree are checked before anything is retried.
    LeftRunning,
    /// `hrafnar` was stopped by this signal: 128 plus its number.
    Signalled(i32),
}

impl CommandExit {
    pub fn code(self) -> i32 {
        match self {
            Self::Success => 0,
            Self::Failed => 1,
            Self::Usage => 2,
            Self::LeftRunning => 3,
            Self::Signalled(sig) => 128 + sig,
        }
    }

    /// The status of a finished run or message. Leftovers win: a failed run is
    /// retried or resumed, but with its processes possibly still changing the
    /// tree, the first thing to do is check them, whatever the answers were.
    pub fn of_runs(failed: bool, left_running: bool) -> Self {
        if left_running {
            Self::LeftRunning
        } else if failed {
            Self::Failed
        } else {
            Self::Success
        }
    }
}

impl From<CommandExit> for std::process::ExitCode {
    fn from(exit: CommandExit) -> Self {
        // Every code is in 0..=255: signals are below 128.
        Self::from(u8::try_from(exit.code()).unwrap_or(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leftovers_outrank_a_failed_run() {
        assert_eq!(CommandExit::of_runs(false, false), CommandExit::Success);
        assert_eq!(CommandExit::of_runs(true, false).code(), 1);
        assert_eq!(CommandExit::of_runs(false, true).code(), 3);
        assert_eq!(CommandExit::of_runs(true, true).code(), 3);
    }

    #[test]
    fn signals_use_the_shell_encoding() {
        assert_eq!(CommandExit::Signalled(libc::SIGINT).code(), 130);
        assert_eq!(CommandExit::Signalled(libc::SIGTERM).code(), 143);
        assert_eq!(CommandExit::Usage.code(), 2);
    }
}
