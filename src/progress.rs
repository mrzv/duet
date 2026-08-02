use color_eyre::eyre::Result;
use std::ops::Deref;
use std::time::Duration;

const BAR_TEMPLATE: &str = "[{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len} {wide_msg}";
const BYTE_BAR_TEMPLATE: &str =
    "[{elapsed_precise}] {bar:40.cyan/blue} {bytes}/{total_bytes} {wide_msg}";
const SPINNER_TEMPLATE: &str = "[{elapsed_precise}] {spinner:.cyan} {pos} {wide_msg}";

pub(crate) fn count_bar(len: u64, message: impl Into<String>) -> Result<indicatif::ProgressBar> {
    let progress = indicatif::ProgressBar::new(len);
    progress.set_style(
        indicatif::ProgressStyle::default_bar()
            .template(BAR_TEMPLATE)?
            .progress_chars("##-"),
    );
    progress.set_message(message.into());
    Ok(progress)
}

pub(crate) fn bytes_bar(
    total_bytes: u64,
    message: impl Into<String>,
) -> Result<indicatif::ProgressBar> {
    let progress = indicatif::ProgressBar::new(total_bytes);
    progress.set_style(
        indicatif::ProgressStyle::default_bar()
            .template(BYTE_BAR_TEMPLATE)?
            .progress_chars("##-"),
    );
    progress.set_message(message.into());
    Ok(progress)
}

pub(crate) fn spinner(message: impl Into<String>) -> Result<indicatif::ProgressBar> {
    let progress = indicatif::ProgressBar::new_spinner();
    progress.set_style(indicatif::ProgressStyle::default_spinner().template(SPINNER_TEMPLATE)?);
    progress.set_message(message.into());
    Ok(progress)
}

pub(crate) struct AutoClearProgressBar(indicatif::ProgressBar);

impl Deref for AutoClearProgressBar {
    type Target = indicatif::ProgressBar;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for AutoClearProgressBar {
    fn drop(&mut self) {
        self.0.finish_and_clear();
    }
}

pub(crate) fn steady_spinner(message: impl Into<String>) -> Result<AutoClearProgressBar> {
    let progress = spinner(message)?;
    progress.enable_steady_tick(Duration::from_millis(100));
    Ok(AutoClearProgressBar(progress))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_bar_sets_length_and_message() {
        let progress = count_bar(7, "working").unwrap();
        assert_eq!(progress.length(), Some(7));
        assert_eq!(progress.message(), "working");
    }

    #[test]
    fn spinner_has_no_fixed_length() {
        let progress = spinner("scanning").unwrap();
        assert_eq!(progress.length(), None);
        assert_eq!(progress.message(), "scanning");
    }

    #[test]
    fn steady_spinner_is_auto_clearing_and_keeps_its_message() {
        let progress = steady_spinner("validating").unwrap();
        let observed = progress.0.clone();
        assert_eq!(progress.length(), None);
        assert_eq!(progress.message(), "validating");
        assert!(!observed.is_finished());

        drop(progress);

        assert!(observed.is_finished());
    }

    #[test]
    fn bytes_bar_sets_length_and_message() {
        let progress = bytes_bar(4096, "streaming").unwrap();
        assert_eq!(progress.length(), Some(4096));
        assert_eq!(progress.message(), "streaming");
    }
}
