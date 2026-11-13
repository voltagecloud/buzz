//! A scripted terminal for setup tests: each prompt must match the next
//! expected label, and any unexpected prompt fails the test.

use std::collections::VecDeque;

use super::profile::Prompt;
use crate::error::CliError;

#[derive(Default)]
pub(super) struct Script {
    steps: VecDeque<(&'static str, Option<&'static str>)>,
    pub notes: Vec<String>,
}

impl Script {
    /// `steps` are (label prefix, answer); `None` presses Enter.
    pub(super) fn new(steps: &[(&'static str, Option<&'static str>)]) -> Self {
        Self {
            steps: steps.iter().copied().collect(),
            notes: Vec::new(),
        }
    }

    fn next(&mut self, label: &str) -> Option<&'static str> {
        let (expected, answer) = self
            .steps
            .pop_front()
            .unwrap_or_else(|| panic!("unexpected prompt: {label}"));
        assert!(
            label.starts_with(expected),
            "expected {expected:?}, prompted {label:?}"
        );
        answer
    }

    pub(super) fn finished(&self) -> bool {
        self.steps.is_empty()
    }
}

impl Prompt for Script {
    fn note(&mut self, text: &str) {
        self.notes.push(text.to_owned());
    }

    fn line(&mut self, label: &str, default: Option<&str>) -> Result<String, CliError> {
        Ok(self
            .next(label)
            .map(str::to_owned)
            .unwrap_or_else(|| default.unwrap_or_default().to_owned()))
    }

    fn secret(&mut self, label: &str) -> Result<Option<String>, CliError> {
        Ok(self.next(label).map(str::to_owned))
    }
}
