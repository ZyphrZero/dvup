use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    fs,
    path::PathBuf,
};

use toml::Value;

const CATALOGS: [(&str, &str); 2] = [
    ("en", "assets/i18n/en.toml"),
    ("zh-CN", "assets/i18n/zh-CN.toml"),
];

fn main() {
    let catalog = verify_catalogs();
    generate_message_keys(&catalog);
    for (_, path) in CATALOGS {
        println!("cargo:rerun-if-changed={path}");
    }
}

fn verify_catalogs() -> BTreeMap<String, String> {
    let catalogs = CATALOGS
        .map(|(language, path)| (language, read_catalog(path)))
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    let english = catalogs.get("en").expect("English catalog is configured");
    let english_keys = english.keys().cloned().collect::<BTreeSet<_>>();

    for (language, catalog) in &catalogs {
        let keys = catalog.keys().cloned().collect::<BTreeSet<_>>();
        let missing = english_keys.difference(&keys).collect::<Vec<_>>();
        let extra = keys.difference(&english_keys).collect::<Vec<_>>();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "i18n key mismatch for {language}: missing {missing:?}; extra {extra:?}"
        );

        for key in &english_keys {
            let source = english.get(key).expect("key comes from English catalog");
            let translation = catalog.get(key).expect("catalog key sets match");
            assert!(
                !translation.is_empty(),
                "i18n value `{key}` is empty in {language}"
            );
            let expected = placeholders(source);
            let actual = placeholders(translation);
            assert_eq!(
                actual, expected,
                "i18n placeholder mismatch for `{key}` in {language}"
            );
        }
    }
    english.clone()
}

fn generate_message_keys(catalog: &BTreeMap<String, String>) {
    let mut generated = String::from(
        "#[derive(Clone, Copy, Debug, Eq, PartialEq)]\n\
         pub(crate) struct MessageKey(&'static str);\n\n\
         impl MessageKey {\n\
             pub(crate) const fn as_str(self) -> &'static str { self.0 }\n\
",
    );
    for (index, key) in catalog.keys().enumerate() {
        writeln!(
            &mut generated,
            "    pub(crate) const KEY_{index}: Self = Self({key:?});"
        )
        .expect("writing to String cannot fail");
    }
    generated.push_str("}\n\nmacro_rules! message_key {\n");
    for (index, key) in catalog.keys().enumerate() {
        writeln!(
            &mut generated,
            "    ({key:?}) => {{ $crate::i18n::MessageKey::KEY_{index} }};"
        )
        .expect("writing to String cannot fail");
    }
    generated.push_str(
        "    ($unknown:literal) => {\n\
             compile_error!(concat!(\"unknown i18n key `\", $unknown, \"`\"))\n\
         };\n\
         }\n\
         pub(crate) use message_key;\n",
    );
    let output = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"))
        .join("i18n_keys.rs");
    fs::write(&output, generated)
        .unwrap_or_else(|error| panic!("failed to write {}: {error}", output.display()));
}

fn read_catalog(path: &str) -> BTreeMap<String, String> {
    let contents =
        fs::read_to_string(path).unwrap_or_else(|error| panic!("failed to read {path}: {error}"));
    let value = toml::from_str::<Value>(&contents)
        .unwrap_or_else(|error| panic!("failed to parse {path}: {error}"));
    let mut messages = BTreeMap::new();
    flatten("", &value, &mut messages);
    messages
}

fn flatten(prefix: &str, value: &Value, messages: &mut BTreeMap<String, String>) {
    match value {
        Value::Table(table) => {
            for (key, value) in table {
                let key = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(&key, value, messages);
            }
        }
        Value::String(value) => {
            messages.insert(prefix.to_owned(), value.clone());
        }
        _ => panic!("i18n catalog values must be strings: {prefix}"),
    }
}

fn placeholders(value: &str) -> BTreeSet<String> {
    let mut placeholders = BTreeSet::new();
    let bytes = value.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        if bytes[start] != b'{' {
            start += 1;
            continue;
        }
        let mut end = start + 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end > start + 1 && end < bytes.len() && bytes[end] == b'}' {
            placeholders.insert(value[start..=end].to_owned());
            start = end + 1;
        } else {
            start += 1;
        }
    }
    placeholders
}
