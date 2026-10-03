//! Presentation locale is independent from the analyzed source language.
use anyhow::{bail, Result};
use std::sync::atomic::{AtomicU8, Ordering};
static LOCALE: AtomicU8 = AtomicU8::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Locale {
    ZhCn,
    En,
}
impl Locale {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "zh-CN" | "zh_CN" | "zh" => Ok(Self::ZhCn),
            "en" | "en-US" | "en_US" => Ok(Self::En),
            _ => bail!("unsupported locale {value}; use zh-CN or en"),
        }
    }
    pub fn tag(self) -> &'static str {
        match self {
            Self::ZhCn => "zh-CN",
            Self::En => "en",
        }
    }
}
pub fn set_locale(value: &str) -> Result<()> {
    let locale = Locale::parse(value)?;
    LOCALE.store(u8::from(locale == Locale::En), Ordering::Relaxed);
    Ok(())
}
pub fn current() -> Locale {
    if LOCALE.load(Ordering::Relaxed) == 1 {
        Locale::En
    } else {
        Locale::ZhCn
    }
}
pub fn is_english() -> bool {
    current() == Locale::En
}

/// Select a system label or format template. Source excerpts and user content
/// must never pass through localization or machine translation.
#[macro_export]
macro_rules! localize {
    ($zh:literal,$en:literal,) => {if $crate::i18n::is_english() {format!($en)} else {format!($zh)}};
    ($zh:expr,$en:expr) => {if $crate::i18n::is_english() {$en} else {$zh}};
    ($zh:literal,$en:literal,$($arguments:tt)+) => {if $crate::i18n::is_english() {format!($en,$($arguments)+)} else {format!($zh,$($arguments)+)}};
}
