//! Is the session locked?
//!
//! A locked session is the quiet failure mode: the compositor either suspends
//! capture (the call times out) or hands back the greeter's frame, which looks
//! like a desktop with nothing on it. Both used to be reported as success.
//!
//! `loginctl show-session <id> -p LockedHint` is the authoritative answer from
//! logind. It is an external command, so it can be missing; then the report is
//! `unknown` with the reason instead of a guess. The greeter process is not
//! evidence — cosmic-greeter-daemon and cosmic-greeter run all the time, locked
//! or not.

use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockState {
    Locked,
    Unlocked,
    Unknown,
}

#[derive(Clone, Debug)]
pub struct LockReport {
    pub state: LockState,
    pub evidence: String,
}

impl LockReport {
    /// `Some(true)` locked, `Some(false)` unlocked, `None` unknown.
    pub fn locked(&self) -> Option<bool> {
        match self.state {
            LockState::Locked => Some(true),
            LockState::Unlocked => Some(false),
            LockState::Unknown => None,
        }
    }
}

pub fn detect() -> LockReport {
    let Some(session) = std::env::var_os("XDG_SESSION_ID") else {
        return unknown("XDG_SESSION_ID is not set, so logind cannot be asked about this session");
    };
    let session = session.to_string_lossy().into_owned();

    match Command::new("loginctl")
        .args(["show-session", &session, "-p", "LockedHint"])
        .output()
    {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            match parse_locked_hint(&text) {
                Some(true) => LockReport {
                    state: LockState::Locked,
                    evidence: format!("loginctl reports LockedHint=yes for session {session}"),
                },
                Some(false) => LockReport {
                    state: LockState::Unlocked,
                    evidence: format!("loginctl reports LockedHint=no for session {session}"),
                },
                None => unknown(&format!(
                    "loginctl answered for session {session} but without a usable LockedHint: {:?}",
                    text.trim()
                )),
            }
        }
        Ok(output) => unknown(&format!(
            "loginctl exited with {} for session {session}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Err(err) => unknown(&format!("could not run loginctl: {err}")),
    }
}

fn unknown(reason: &str) -> LockReport {
    LockReport {
        state: LockState::Unknown,
        evidence: reason.to_string(),
    }
}

fn parse_locked_hint(text: &str) -> Option<bool> {
    for line in text.lines() {
        if let Some(value) = line.trim().strip_prefix("LockedHint=") {
            return match value.trim() {
                "yes" => Some(true),
                "no" => Some(false),
                _ => None,
            };
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_two_answers_logind_gives() {
        assert_eq!(
            parse_locked_hint(
                "LockedHint=yes
"
            ),
            Some(true)
        );
        assert_eq!(
            parse_locked_hint(
                "LockedHint=no
"
            ),
            Some(false)
        );
    }

    #[test]
    fn ignores_unrelated_properties_and_rejects_nonsense() {
        assert_eq!(
            parse_locked_hint(
                "Id=4
LockedHint=no
Active=yes
"
            ),
            Some(false)
        );
        assert_eq!(
            parse_locked_hint(
                "LockedHint=maybe
"
            ),
            None
        );
        assert_eq!(
            parse_locked_hint(
                "Id=4
"
            ),
            None
        );
    }

    #[test]
    fn unknown_is_not_reported_as_unlocked() {
        let report = unknown("loginctl is missing");
        assert_eq!(report.state, LockState::Unknown);
        assert_eq!(report.locked(), None);
    }
}
