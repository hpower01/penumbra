/*
    SPDX-License-Identifier: AGPL-3.0-or-later
    SPDX-FileCopyrightText: 2026 Shomy
*/
use std::cell::RefCell;
use std::sync::OnceLock;

use colored::Colorize;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::logger::{INFO_SYMBOL, LOGGER_PREIX};

/// Shared container used in multi-device mode so that every per-device progress
/// bar is rendered on its own line without clobbering the others. When unset
/// (single-device/CLI mode), [`AntumbraProgress`] behaves exactly as before.
static MULTI: OnceLock<MultiProgress> = OnceLock::new();

thread_local! {
    /// Label identifying the device the current thread is flashing. Each
    /// worker thread sets this once so its progress bar is attributed to the
    /// right device.
    static DEVICE_LABEL: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Enables multi-device progress rendering and returns the shared
/// [`MultiProgress`]. Idempotent: repeated calls return the same instance.
pub fn enable_multi_progress() -> &'static MultiProgress {
    MULTI.get_or_init(MultiProgress::new)
}

/// Associates the calling thread with a device label, shown as a prefix on any
/// progress bar it creates afterwards.
pub fn set_device_label(label: impl Into<String>) {
    DEVICE_LABEL.with(|l| *l.borrow_mut() = Some(label.into()));
}

fn current_device_label() -> Option<String> {
    DEVICE_LABEL.with(|l| l.borrow().clone())
}

/// A wrapper around indicatif ProgressBar
/// With custom styling from the logger
pub struct AntumbraProgress {
    pb: ProgressBar,
    #[allow(dead_code)]
    prefix: String,
}

impl AntumbraProgress {
    pub fn new(total_size: u64) -> Self {
        let logger_prefix = format!("{} {}", LOGGER_PREIX.bold().purple(), INFO_SYMBOL.purple());

        // In multi-device mode, tag each bar with the device it belongs to so
        // several concurrent flashes stay readable.
        let device_tag = current_device_label()
            .map(|label| format!("{} ", format!("[{label}]").bold().cyan()))
            .unwrap_or_default();

        let prefix = format!("{logger_prefix} {device_tag}");

        let pb = match MULTI.get() {
            Some(multi) => multi.add(ProgressBar::new(total_size)),
            None => ProgressBar::new(total_size),
        };

        pb.set_style(
            ProgressStyle::with_template(
                &format!(
                     "{}  [{{bar:40.magenta/red}}] {{bytes}}/{{total_bytes}} ({{elapsed}} / ETA: {{eta}}, {{bytes_per_sec}}) {{msg}}",
                     prefix
                 )
            )
            .unwrap()
            .progress_chars("##-"),
        );

        Self { pb, prefix }
    }

    pub fn update(&self, written: u64, msg: &str) {
        self.pb.set_position(written);
        self.pb.set_message(msg.to_string());
    }

    pub fn finish(&self, msg: &str) {
        self.pb.finish_with_message(msg.to_string());
    }

    pub fn abandon(&self, msg: &str) {
        self.pb.abandon_with_message(msg.to_string());
    }

    pub fn set_total(&self, total: u64) {
        if self.pb.length() != Some(total) {
            self.pb.set_length(total);
        }
    }

    pub fn get_callback<'a>(
        &'a self,
        running_msg: &'a str,
        finished_msg: &'a str,
    ) -> impl FnMut(u64, u64) + 'a {
        move |written: u64, total: u64| {
            self.update(written, running_msg);

            if written >= total {
                self.finish(finished_msg);
            }
        }
    }
}
