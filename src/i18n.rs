//! TOML-backed localization for user-facing dvup text.

use std::{collections::HashMap, fmt::Display, sync::LazyLock};

use crate::settings::Language;

include!(concat!(env!("OUT_DIR"), "/i18n_keys.rs"));

static ENGLISH: LazyLock<HashMap<String, String>> =
    LazyLock::new(|| parse_catalog("en", include_str!("../assets/i18n/en.toml")));
static CHINESE: LazyLock<HashMap<String, String>> =
    LazyLock::new(|| parse_catalog("zh-CN", include_str!("../assets/i18n/zh-CN.toml")));

pub(crate) fn t(language: Language, key: MessageKey) -> &'static str {
    let key = key.as_str();
    catalog(language)
        .get(key)
        .unwrap_or_else(|| panic!("missing i18n key `{key}` in {}", language.code()))
        .as_str()
}

pub(crate) fn tr(language: Language, key: MessageKey, args: &[&dyn Display]) -> String {
    let arguments = args.iter().map(ToString::to_string).collect::<Vec<_>>();
    interpolate(t(language, key), &arguments)
}

fn interpolate(template: &str, arguments: &[String]) -> String {
    let mut output = String::with_capacity(template.len());
    let mut cursor = 0;
    while let Some(relative_start) = template[cursor..].find('{') {
        let start = cursor + relative_start;
        output.push_str(&template[cursor..start]);
        let suffix = &template[start + 1..];
        let digit_count = suffix
            .as_bytes()
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        let closes_placeholder = digit_count > 0
            && suffix
                .as_bytes()
                .get(digit_count)
                .is_some_and(|byte| *byte == b'}');
        if closes_placeholder {
            let end = start + digit_count + 2;
            let index = suffix[..digit_count]
                .parse::<usize>()
                .expect("placeholder contains only ASCII digits");
            if let Some(argument) = arguments.get(index) {
                output.push_str(argument);
                cursor = end;
                continue;
            }
        }
        output.push('{');
        cursor = start + 1;
    }
    output.push_str(&template[cursor..]);
    output
}

impl Language {
    pub(crate) fn text(self, key: MessageKey) -> &'static str {
        t(self, key)
    }

    pub(crate) fn format(self, key: MessageKey, args: &[&dyn Display]) -> String {
        tr(self, key, args)
    }

    pub(crate) fn list<const N: usize>(self, keys: [MessageKey; N]) -> [&'static str; N] {
        keys.map(|key| self.text(key))
    }

    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::Chinese => "zh-CN",
        }
    }
}

fn catalog(language: Language) -> &'static HashMap<String, String> {
    match language {
        Language::English => &ENGLISH,
        Language::Chinese => &CHINESE,
    }
}

fn parse_catalog(language: &str, contents: &str) -> HashMap<String, String> {
    let value = toml::from_str::<toml::Value>(contents)
        .unwrap_or_else(|error| panic!("failed to parse {language} i18n catalog: {error}"));
    let mut messages = HashMap::new();
    flatten("", &value, &mut messages);
    messages
}

fn flatten(prefix: &str, value: &toml::Value, messages: &mut HashMap<String, String>) {
    match value {
        toml::Value::Table(table) => {
            for (key, value) in table {
                let key = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(&key, value, messages);
            }
        }
        toml::Value::String(value) => {
            messages.insert(prefix.to_owned(), value.clone());
        }
        _ => panic!("i18n catalog values must be strings: {prefix}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_languages_resolve_the_same_message_key() {
        assert_eq!(t(Language::English, message_key!("common.ready")), "Ready");
        assert_eq!(t(Language::Chinese, message_key!("common.ready")), "就绪");
    }

    #[test]
    fn numbered_arguments_are_interpolated_in_catalog_text() {
        assert_eq!(
            tr(
                Language::English,
                message_key!("release.probe_summary"),
                &[&3, &1]
            ),
            "GitHub repositories refreshed: 3 update(s), 1 failed"
        );
        assert_eq!(
            tr(
                Language::Chinese,
                message_key!("release.probe_summary"),
                &[&3, &1]
            ),
            "GitHub 仓库刷新完成：3 项可更新，1 项失败"
        );
    }

    #[test]
    fn interpolation_does_not_treat_inserted_arguments_as_template_syntax() {
        assert_eq!(
            tr(
                Language::English,
                message_key!("release.probe_summary"),
                &[&"tool {1}", &1],
            ),
            "GitHub repositories refreshed: tool {1} update(s), 1 failed"
        );
    }
}
