//! Parsers for the module domain.

use super::*;

/// Extract the `modules` map from `config.php` source, preserving declaration order
/// (which is Magento's authoritative, already-sequence-resolved load order).
pub(crate) fn config_php_modules(src: &str) -> Result<Vec<(ModuleName, bool)>, String> {
    // PHP treats `'modules'` and `"modules"` as the same string, and so must we: Magento's
    // own `var_export` writes single quotes, but a hand-edited file (or one a deployment
    // tool reformatted) may use double ones. Take whichever spelling comes first.
    let key = ["'modules'", "\"modules\""]
        .iter()
        .filter_map(|needle| src.find(needle))
        .min()
        .ok_or("no `modules` key found in config.php")?;
    // The value opens with either short `[` (what Magento writes) or long `array (` — the
    // form PHP's own `var_export` produces, valid in any case and spacing. Read the opener
    // right after the `=>` rather than searching ahead for a `[`: a long-form file has
    // none, and a search would run off into whatever array came next.
    let arrow = src[key..]
        .find("=>")
        .map(|i| key + i + 2)
        .ok_or("malformed 'modules' entry (no `=>`)")?;
    let rest = src[arrow..].trim_start();
    let (open, open_b, close_b) = if rest.starts_with('[') {
        (src.len() - rest.len(), b'[', b']')
    } else if rest.get(..5).is_some_and(|w| w.eq_ignore_ascii_case("array"))
        && rest[5..].trim_start().starts_with('(')
    {
        (src.len() - rest[5..].trim_start().len(), b'(', b')')
    } else {
        return Err("malformed 'modules' entry (no opening `[` or `array(`)".into());
    };

    // The modules block is flat (values are 0/1, keys are simple quoted identifiers),
    // so a depth scan over the opener's own bracket pair reaches the matching close safely.
    let bytes = src.as_bytes();
    let mut depth = 0i32;
    let mut close = None;
    for (offset, &b) in bytes[open..].iter().enumerate() {
        if b == open_b {
            depth += 1;
        } else if b == close_b {
            depth -= 1;
            if depth == 0 {
                close = Some(open + offset);
                break;
            }
        }
    }
    let close = close.ok_or("unterminated 'modules' array")?;
    let block = &src[open + 1..close];

    let mut out = Vec::new();
    for entry in block.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (k, v) = entry
            .split_once("=>")
            .ok_or_else(|| format!("malformed module entry: `{entry}`"))?;
        // Both quote styles again: a `"`-quoted name that kept its quotes would never match
        // a module on disk, and every module would silently read as missing.
        let name = k.trim().trim_matches(['\'', '"']).trim();
        if name.is_empty() {
            continue;
        }
        // The value carries the same risk as the key: a hand-edited `=> '1'` read as
        // disabled would switch a live module off in every answer, silently. PHP's own
        // truthiness is wider than this, but config.php's vocabulary is not — Magento
        // writes the bare ints 0 and 1, and the only other spellings a human reaches for
        // are those quoted, or `true`/`false`. Everything else stays disabled, as before.
        let value = v.trim().trim_matches(['\'', '"']).trim();
        let enabled = value == "1" || value.eq_ignore_ascii_case("true");
        out.push((ModuleName::new(name), enabled));
    }
    Ok(out)
}

pub(crate) struct ParsedModuleXml {
    pub name: ModuleName,
    pub sequence: Vec<ModuleName>,
}

/// Parse `<module name="...">` and its `<sequence><module name="..."/></sequence>` children.
pub(crate) fn module_xml(xml: &str) -> Result<ParsedModuleXml, String> {
    let mut reader = Reader::from_str(xml);
    let mut name: Option<ModuleName> = None;
    let mut sequence = Vec::new();
    let mut in_sequence = false;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Err(e) => return Err(e.to_string()),
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match e.name().as_ref() {
                b"sequence" => in_sequence = true,
                b"module" => {
                    if in_sequence {
                        if let Some(n) = attr(&e, b"name") {
                            sequence.push(ModuleName::new(n));
                        }
                    } else if name.is_none() {
                        name = attr(&e, b"name").map(ModuleName::new);
                    }
                }
                _ => {}
            },
            Ok(Event::End(e)) => {
                if e.name().as_ref() == b"sequence" {
                    in_sequence = false;
                }
            }
            _ => {}
        }
        buf.clear();
    }

    let name = name.ok_or("no <module name=\"...\"> element")?;
    Ok(ParsedModuleXml { name, sequence })
}

#[cfg(test)]
mod module_tests {
    use super::config_php_modules;

    /// `(name, enabled)` pairs as plain strings, for readable assertions.
    fn parse(src: &str) -> Vec<(String, bool)> {
        config_php_modules(src)
            .unwrap()
            .into_iter()
            .map(|(n, e)| (n.as_str().to_owned(), e))
            .collect()
    }

    /// The control: what Magento's own `var_export` writes. Also pins the two properties
    /// the whole hand-rolled scan exists for — declaration order (Magento's authoritative,
    /// already-sequence-resolved load order) and the 0/1 enabled flag.
    #[test]
    fn single_quoted_is_parsed_in_declaration_order() {
        let src = r#"<?php
return [
    'modules' => [
        'Magento_Store' => 1,
        'Acme_Zebra' => 1,
        'Acme_Alpha' => 0,
    ]
];
"#;
        assert_eq!(
            parse(src),
            [
                ("Magento_Store".to_owned(), true),
                // Not alphabetical, and must not become so: config.php's order IS the load
                // order, and sorting it would silently reorder every merge downstream.
                ("Acme_Zebra".to_owned(), true),
                ("Acme_Alpha".to_owned(), false),
            ]
        );
    }

    /// PHP accepts both quote styles. A `"modules"` key used to be invisible to the scan,
    /// so `Magento::open` failed outright and every command refused to run.
    #[test]
    fn double_quoted_key_is_found() {
        let src = "<?php\nreturn [\"modules\" => ['Acme_One' => 1]];\n";
        assert_eq!(parse(src), [("Acme_One".to_owned(), true)]);
    }

    /// The quieter half: the key was found, but a `"`-quoted name kept its quotes, so the
    /// parsed `ModuleName` was `"Acme_One"` (quotes included) and matched nothing on disk —
    /// every module read as "listed in config.php but no module.xml found", with no error.
    #[test]
    fn double_quoted_module_names_keep_no_quotes() {
        let src = "<?php\nreturn ['modules' => [\"Acme_One\" => 1, \"Acme_Two\" => 0]];\n";
        assert_eq!(
            parse(src),
            [("Acme_One".to_owned(), true), ("Acme_Two".to_owned(), false)]
        );
    }

    /// A hand-edited or tool-reformatted file may mix the styles within one block; each
    /// entry is trimmed on its own, so the mix must parse like either pure form, in order.
    #[test]
    fn mixed_quote_styles_parse_and_keep_order() {
        let src = r#"<?php
return ["modules" => [
    'Magento_Store' => 1,
    "Acme_Zebra" => 1,
    'Acme_Alpha' => 0,
]];
"#;
        assert_eq!(
            parse(src),
            [
                ("Magento_Store".to_owned(), true),
                ("Acme_Zebra".to_owned(), true),
                ("Acme_Alpha".to_owned(), false),
            ]
        );
    }

    /// The key search takes whichever spelling comes FIRST in the file, not whichever of the
    /// two we happen to look for first — otherwise a later, unrelated `'modules'` string
    /// (here a plain value) would win over the real `"modules"` node and the scan would run
    /// off into the wrong array.
    #[test]
    fn the_earlier_spelling_wins() {
        let src = r#"<?php
return [
    "modules" => ['Acme_One' => 1],
    'x-generated-by' => ['node' => 'modules'],
];
"#;
        assert_eq!(parse(src), [("Acme_One".to_owned(), true)]);
    }

    /// A quoted value is the key bug's twin: `'Acme_One' => '1'` is enabled in PHP, and
    /// reading it as disabled drops a live module from every answer without a word.
    #[test]
    fn a_quoted_value_still_means_enabled() {
        let src = r#"<?php
return [
    'modules' => [
        'Acme_One' => '1',
        'Acme_Two' => "1",
        'Acme_Off' => '0',
        'Acme_Plain' => 1,
    ]
];
"#;
        assert_eq!(
            parse(src),
            [
                ("Acme_One".to_owned(), true),
                ("Acme_Two".to_owned(), true),
                ("Acme_Off".to_owned(), false),
                ("Acme_Plain".to_owned(), true),
            ]
        );
    }

    /// `true`/`false` are what a hand edit reaches for when it does not reach for 1/0.
    #[test]
    fn a_bareword_boolean_value_is_honored() {
        let src = r#"<?php
return ['modules' => ['Acme_On' => true, 'Acme_Off' => false, 'Acme_Shouty' => TRUE]];
"#;
        assert_eq!(
            parse(src),
            [
                ("Acme_On".to_owned(), true),
                ("Acme_Off".to_owned(), false),
                ("Acme_Shouty".to_owned(), true),
            ]
        );
    }

    /// Deliberately NOT PHP truthiness: `2` and a non-empty string are truthy in PHP, but
    /// nothing writes them here, and guessing a module into the enabled set is the costlier
    /// mistake. Anything outside the known vocabulary stays disabled.
    #[test]
    fn an_unrecognized_value_stays_disabled() {
        let src = r#"<?php
return ['modules' => ['Acme_Odd' => 2, 'Acme_Weird' => 'yes', 'Acme_Null' => null]];
"#;
        assert_eq!(
            parse(src),
            [
                ("Acme_Odd".to_owned(), false),
                ("Acme_Weird".to_owned(), false),
                ("Acme_Null".to_owned(), false),
            ]
        );
    }

    /// The long-array form PHP's `var_export` writes (`array (`), and the other spellings
    /// PHP accepts for it. A file in this shape has no `[` at all, so the scan used to fail
    /// with "no opening `[`" and `Magento::open` refused to run.
    #[test]
    fn long_array_syntax_is_parsed() {
        for opener in ["array (", "array(", "ARRAY(", "Array  ("] {
            let src = format!(
                "<?php\nreturn {opener}'modules' => {opener}'Acme_Zebra' => 1, 'Acme_Alpha' => 0,),);\n"
            );
            assert_eq!(
                parse(&src),
                [("Acme_Zebra".to_owned(), true), ("Acme_Alpha".to_owned(), false)],
                "opener {opener:?}"
            );
        }
    }

    /// Each array closes on its own bracket pair, so long form nested in a short top level
    /// (and the reverse) must parse like either pure form, and stop at the right close.
    #[test]
    fn long_and_short_array_syntax_mix() {
        let long_inner = "<?php\nreturn ['modules' => array('Acme_One' => 1), 'x' => [1]];\n";
        assert_eq!(parse(long_inner), [("Acme_One".to_owned(), true)]);
        let long_outer = "<?php\nreturn array('modules' => ['Acme_One' => 1], 'x' => array(1));\n";
        assert_eq!(parse(long_outer), [("Acme_One".to_owned(), true)]);
    }

    /// The whole file exactly as `var_export` lays it out: the value on the line after the
    /// `=>`, and a trailing comma inside every array.
    #[test]
    fn a_var_export_written_file_is_parsed() {
        let src = "<?php\nreturn array (\n  'modules' => \n  array (\n    'A_B' => 1,\n  ),\n);\n";
        assert_eq!(parse(src), [("A_B".to_owned(), true)]);
    }

    /// A `modules` value that is neither opener is an error, not a silently empty list.
    #[test]
    fn a_modules_value_that_is_not_an_array_is_an_error() {
        let err = config_php_modules("<?php\nreturn ['modules' => null];\n").unwrap_err();
        assert!(err.contains("no opening"), "unexpected error: {err}");
    }

    /// No `modules` node at all is still an error — in either spelling.
    #[test]
    fn a_missing_modules_node_is_an_error() {
        let err = config_php_modules("<?php\nreturn ['system' => []];\n").unwrap_err();
        assert!(err.contains("modules"), "unexpected error: {err}");
    }
}
