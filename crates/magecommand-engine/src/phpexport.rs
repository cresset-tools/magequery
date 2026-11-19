//! Byte-exact `var_export` serialization — the format Magento writes its
//! compiled metadata in (`<?php return array ( … );`). Parity is byte-level,
//! so every quirk is deliberate: the trailing space after `=>` on array
//! lines, nested `array (` at the key's indent, uppercase `NULL`, `\` and
//! `'` as the only string escapes, and no trailing newline after `);`.

/// A PHP value as var_export represents it.
#[derive(Debug, Clone, PartialEq)]
pub enum PhpValue {
    Str(String),
    Int(i64),
    /// var_export prints floats at shortest-roundtrip precision, with a
    /// `.0` suffix on whole values.
    Float(f64),
    Bool(bool),
    Null,
    /// A verbatim PHP expression emitted UNQUOTED — a class-constant / enum-case
    /// reference like `\Vendor\Enum::CASE`. Magento's compiled config keeps a
    /// constructor default that is an enum case as the constant reference (an
    /// enum case is an object, not a scalar it can fold). The caller is
    /// responsible for the string already being valid, fully-qualified PHP.
    Raw(String),
    /// Entries in output order — the caller owns ordering (ksort or
    /// insertion order, whatever the Magento generator being mirrored does).
    Array(Vec<(PhpKey, PhpValue)>),
    /// An object: `\Fqcn::__set_state(array(…))` with its initialized
    /// properties in PHP's property-table order. Names may repeat — a
    /// parent's private property and a child's same-named one are two slots,
    /// and var_export prints both.
    Object {
        /// Declared-case FQCN, no leading backslash.
        class: String,
        props: Vec<(String, PhpValue)>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum PhpKey {
    Int(i64),
    Str(String),
}

impl PhpValue {
    pub fn str(s: impl Into<String>) -> Self {
        PhpValue::Str(s.into())
    }
}

impl PhpKey {
    pub fn str(s: impl Into<String>) -> Self {
        PhpKey::Str(s.into())
    }
}

/// The full metadata file: `<?php return <var_export>;` — no trailing newline.
pub fn to_php_file(value: &PhpValue) -> String {
    let mut out = String::with_capacity(4096);
    out.push_str("<?php return ");
    export(value, 0, &mut out);
    out.push(';');
    out
}

/// Emit a `var_export` single-quoted string literal (only `\` and `'` are
/// escaped) — the [`PhpValue::Str`] rendering, factored out so a borrowed `&str`
/// can be serialized without wrapping it in an owned `PhpValue` first.
fn push_php_str(out: &mut String, s: &str) {
    out.push('\'');
    for ch in s.chars() {
        if ch == '\\' || ch == '\'' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('\'');
}

/// Serialize a compiled **area file** (`global.php`, `frontend.php`, …) directly
/// from its four borrowed sections — byte-identical to building a
/// `PhpValue::Array` of `arguments`/`preferences`/`instanceTypes`/`nonLazyTypes`
/// and calling [`to_php_file`], but WITHOUT deep-cloning the (large) argument
/// tree only to serialize and drop it. Each argument value is written in place by
/// reference. Locked byte-for-byte against the old path by `render_matches_clone`
/// below (and, end-to-end, by `di verify`).
pub fn area_file_to_php(
    arguments: &std::collections::BTreeMap<String, PhpValue>,
    preferences: &[(String, String)],
    instance_types: &[(String, String)],
    non_lazy: &[String],
) -> String {
    // Each section renders as `  '<name>' => \n  array (\n <entries@indent 4> \n
    // ),\n` — the enclosing PhpValue::Array places its four (array) values each on
    // its own line at indent 2, exactly as var_export does.
    fn open(out: &mut String, name: &str) {
        out.push_str("  ");
        push_php_str(out, name);
        out.push_str(" => \n  array (\n");
    }
    fn close(out: &mut String) {
        out.push_str("  ),\n");
    }
    fn str_entries(out: &mut String, entries: &[(String, String)]) {
        for (k, v) in entries {
            out.push_str("    ");
            push_php_str(out, k);
            out.push_str(" => ");
            push_php_str(out, v);
            out.push_str(",\n");
        }
    }

    // ~64 bytes/entry is a rough capacity hint (a re-alloc if wrong, never a
    // correctness issue); the argument section dominates.
    let mut out = String::with_capacity(64 * arguments.len() + 4096);
    out.push_str("<?php return array (\n");

    open(&mut out, "arguments");
    for (k, v) in arguments {
        out.push_str("    ");
        push_php_str(&mut out, k);
        out.push_str(" => ");
        // var_export puts an array/object value on its own line at the key indent.
        if on_own_line(v) {
            out.push_str("\n    ");
        }
        export(v, 4, &mut out);
        out.push_str(",\n");
    }
    close(&mut out);

    open(&mut out, "preferences");
    str_entries(&mut out, preferences);
    close(&mut out);

    open(&mut out, "instanceTypes");
    str_entries(&mut out, instance_types);
    close(&mut out);

    open(&mut out, "nonLazyTypes");
    for k in non_lazy {
        out.push_str("    ");
        push_php_str(&mut out, k);
        out.push_str(" => true,\n");
    }
    close(&mut out);

    out.push_str(");");
    out
}

/// var_export starts arrays and objects — including enum cases, a `Raw`
/// bareword like `\Enum::CASE` — on their own line, after a trailing space on
/// the key line.
fn on_own_line(value: &PhpValue) -> bool {
    matches!(value, PhpValue::Array(_) | PhpValue::Raw(_) | PhpValue::Object { .. })
}

fn export(value: &PhpValue, indent: usize, out: &mut String) {
    match value {
        PhpValue::Str(s) => push_php_str(out, s),
        PhpValue::Int(i) => out.push_str(&i.to_string()),
        PhpValue::Float(f) => {
            // Metadata is `var_export` output, and Magento sets
            // `serialize_precision = 14` in app/bootstrap.php:67 before any of
            // it runs — so a float carries 14 SIGNIFICANT digits and
            // `0.1 + 0.2` is written `0.3`, not `0.30000000000000004`.
            //
            // The same 14 governs generated CODE, but via `precision`
            // (bootstrap.php:66) and through a different renderer, and the two
            // disagree on integral floats: code writes `256`, var_export keeps
            // the trailing `.0`.
            if f.fract() == 0.0 && f.is_finite() && f.abs() < 1e15 {
                out.push_str(&format!("{f:.1}"));
            } else {
                out.push_str(&crate::laminas::format_g(*f, 14));
            }
        }
        PhpValue::Bool(true) => out.push_str("true"),
        PhpValue::Bool(false) => out.push_str("false"),
        PhpValue::Null => out.push_str("NULL"),
        PhpValue::Raw(expr) => out.push_str(expr),
        PhpValue::Array(entries) => {
            out.push_str("array (\n");
            let inner = indent + 2;
            for (key, val) in entries {
                for _ in 0..inner {
                    out.push(' ');
                }
                match key {
                    PhpKey::Int(i) => out.push_str(&i.to_string()),
                    PhpKey::Str(s) => export(&PhpValue::Str(s.clone()), inner, out),
                }
                out.push_str(" => ");
                if on_own_line(val) {
                    // var_export puts nested arrays — and objects, which includes
                    // enum cases (a `Raw` bareword like `\Enum::CASE`) — on their
                    // own line at the key's indent, after a trailing space on the
                    // key line.
                    out.push('\n');
                    for _ in 0..inner {
                        out.push(' ');
                    }
                }
                export(val, inner, out);
                out.push_str(",\n");
            }
            for _ in 0..indent {
                out.push(' ');
            }
            out.push(')');
        }
        PhpValue::Object { class, props } => {
            // php_var_export_ex: an object's properties sit ONE column deeper
            // than an array's entries would (`level + 2` against `level + 1`),
            // and a nested value is exported at that same level — so its own
            // line starts one column left of the property names.
            out.push('\\');
            out.push_str(class);
            out.push_str("::__set_state(array(\n");
            for (name, val) in props {
                for _ in 0..indent + 3 {
                    out.push(' ');
                }
                push_php_str(out, name);
                out.push_str(" => ");
                if on_own_line(val) {
                    out.push('\n');
                    for _ in 0..indent + 2 {
                        out.push(' ');
                    }
                }
                export(val, indent + 2, out);
                out.push_str(",\n");
            }
            for _ in 0..indent {
                out.push(' ');
            }
            out.push_str("))");
        }
    }
}

#[cfg(test)]
mod tests {
    /// Magento's metadata is `var_export` output, and `app/bootstrap.php:67`
    /// sets `ini_set('serialize_precision', 14)` before any of it runs — so a
    /// float is written with 14 SIGNIFICANT digits, not PHP's default
    /// round-trip precision. `0.1 + 0.2` is therefore `0.3` in a compiled
    /// metadata file, while a bare `var_export` outside Magento would write
    /// `0.30000000000000004`.
    ///
    /// Found by differential fuzzing. Note this is the same 14 as the
    /// generated-code path but reached by a different ini setting
    /// (`precision` there, `serialize_precision` here), and the two disagree
    /// on integral floats: code writes `256`, metadata writes `256.0`.
    #[test]
    fn floats_use_magentos_serialize_precision_of_14() {
        let f = |v: f64| to_php_file(&PhpValue::Float(v));
        assert!(f(0.1 + 0.2).contains("0.3;"), "got {}", f(0.1 + 0.2));
        assert!(f(1.0 / 3.0).contains("0.33333333333333;"), "got {}", f(1.0 / 3.0));
        // Integral floats keep var_export's trailing `.0`.
        assert!(f(256.0).contains("256.0;"), "got {}", f(256.0));
        assert!(f(-0.0).contains("-0.0;"), "got {}", f(-0.0));
        assert!(f(1000.0).contains("1000.0;"), "got {}", f(1000.0));
        // A value that already needs few digits is unchanged.
        assert!(f(1.5).contains("1.5;"), "got {}", f(1.5));
    }

    use super::*;

    #[test]
    fn scalar_map_matches_var_export() {
        let v = PhpValue::Array(vec![
            (
                PhpKey::str("magento\\a\\b"),
                PhpValue::str("Magento\\A\\B"),
            ),
            (PhpKey::Int(0), PhpValue::Bool(false)),
            (PhpKey::str("n"), PhpValue::Null),
            (PhpKey::str("i"), PhpValue::Int(42)),
        ]);
        let expected = "<?php return array (\n  'magento\\\\a\\\\b' => 'Magento\\\\A\\\\B',\n  0 => false,\n  'n' => NULL,\n  'i' => 42,\n);";
        assert_eq!(to_php_file(&v), expected);
    }

    #[test]
    fn nested_arrays_match_var_export_layout() {
        let v = PhpValue::Array(vec![(
            PhpKey::str("arguments"),
            PhpValue::Array(vec![(
                PhpKey::str("fileName"),
                PhpValue::Array(vec![(PhpKey::str("_v_"), PhpValue::str("x.xml"))]),
            )]),
        )]);
        // Note the trailing space after `=>` before each nested array.
        let expected = "<?php return array (\n  'arguments' => \n  array (\n    'fileName' => \n    array (\n      '_v_' => 'x.xml',\n    ),\n  ),\n);";
        assert_eq!(to_php_file(&v), expected);
    }

    #[test]
    fn raw_renders_unquoted_bareword() {
        // An enum-case default is emitted as a verbatim `\Enum::CASE` reference,
        // NOT a quoted string (G4) — and, like any object, var_export places it on
        // its own line at the key's indent (trailing space after `=>`).
        let v = PhpValue::Array(vec![(
            PhpKey::str("_v_"),
            PhpValue::Raw("\\Vendor\\Enum::CASE".to_owned()),
        )]);
        assert_eq!(
            to_php_file(&v),
            "<?php return array (\n  '_v_' => \n  \\Vendor\\Enum::CASE,\n);"
        );
    }

    /// `['k' => ['_v_' => $obj]]` — the shape an object default takes in a
    /// metadata file.
    fn in_v(obj: PhpValue) -> PhpValue {
        PhpValue::Array(vec![(
            PhpKey::str("k"),
            PhpValue::Array(vec![(PhpKey::str("_v_"), obj)]),
        )])
    }

    fn obj(class: &str, props: Vec<(&str, PhpValue)>) -> PhpValue {
        PhpValue::Object {
            class: class.to_owned(),
            props: props.into_iter().map(|(n, v)| (n.to_owned(), v)).collect(),
        }
    }

    /// Ground truth from PHP 8.4 `var_export` (`php -r`), copied byte for
    /// byte: an object with no properties still opens and closes its
    /// `array(` on separate lines, and `))` closes at the object's indent.
    #[test]
    fn empty_object_matches_var_export() {
        assert_eq!(
            to_php_file(&in_v(obj("N\\E", vec![]))),
            "<?php return array (\n  'k' => \n  array (\n    '_v_' => \n    \\N\\E::__set_state(array(\n    )),\n  ),\n);"
        );
        // Top level: no leading newline, no indent.
        assert_eq!(
            to_php_file(&obj("N\\E", vec![])),
            "<?php return \\N\\E::__set_state(array(\n));"
        );
        // Inside one more array level the whole block shifts by two.
        let nested = PhpValue::Array(vec![(
            PhpKey::str("k"),
            PhpValue::Array(vec![(
                PhpKey::str("_v_"),
                PhpValue::Array(vec![(PhpKey::str("x"), obj("N\\E", vec![]))]),
            )]),
        )]);
        assert_eq!(
            to_php_file(&nested),
            "<?php return array (\n  'k' => \n  array (\n    '_v_' => \n    array (\n      'x' => \n      \\N\\E::__set_state(array(\n      )),\n    ),\n  ),\n);"
        );
    }

    /// PHP 8.4 ground truth for scalar, null, float and nested-array
    /// properties: property names sit at the object's indent + 3 (one column
    /// deeper than array entries), and an array-valued property starts its
    /// own line at indent + 2.
    #[test]
    fn object_properties_match_var_export() {
        let v = in_v(obj(
            "N\\C",
            vec![
                ("pa", PhpValue::Int(1)),
                ("pb", PhpValue::str("x")),
                ("pc", PhpValue::Null),
                (
                    "own",
                    PhpValue::Array(vec![
                        (PhpKey::Int(0), PhpValue::Int(1)),
                        (
                            PhpKey::str("a"),
                            PhpValue::Array(vec![
                                (PhpKey::Int(0), PhpValue::Bool(true)),
                                (PhpKey::Int(1), PhpValue::Null),
                            ]),
                        ),
                        (PhpKey::str("b"), PhpValue::Array(vec![])),
                    ]),
                ),
                ("pc", PhpValue::Float(2.5)),
                ("f", PhpValue::Float(3.0)),
                ("neg", PhpValue::Int(-1)),
            ],
        ));
        // One string per line: the trailing space after `=>` is part of it.
        let expected = [
            "<?php return array (",
            "  'k' => ",
            "  array (",
            "    '_v_' => ",
            "    \\N\\C::__set_state(array(",
            "       'pa' => 1,",
            "       'pb' => 'x',",
            "       'pc' => NULL,",
            "       'own' => ",
            "      array (",
            "        0 => 1,",
            "        'a' => ",
            "        array (",
            "          0 => true,",
            "          1 => NULL,",
            "        ),",
            "        'b' => ",
            "        array (",
            "        ),",
            "      ),",
            "       'pc' => 2.5,",
            "       'f' => 3.0,",
            "       'neg' => -1,",
            "    )),",
            "  ),",
            ");",
        ]
        .join("\n");
        assert_eq!(to_php_file(&v), expected);
    }

    #[test]
    fn quotes_are_escaped() {
        let v = PhpValue::Array(vec![(PhpKey::str("k"), PhpValue::str("it's"))]);
        assert_eq!(
            to_php_file(&v),
            "<?php return array (\n  'k' => 'it\\'s',\n);"
        );
    }

    #[test]
    fn area_file_to_php_matches_wrapper_path() {
        // Lock the reference-based area serializer byte-for-byte against the old
        // "wrap in PhpValue::Array + to_php_file" path, across every value shape:
        // a nested-array argument, a Raw (enum-case) argument, a scalar argument,
        // an empty section, and strings needing escaping.
        use std::collections::BTreeMap;
        let mut arguments: BTreeMap<String, PhpValue> = BTreeMap::new();
        arguments.insert(
            "Magento\\A\\B".to_owned(),
            PhpValue::Array(vec![(
                PhpKey::str("fileName"),
                PhpValue::Array(vec![(PhpKey::str("_v_"), PhpValue::str("x.xml"))]),
            )]),
        );
        arguments.insert(
            "It's\\Odd".to_owned(),
            PhpValue::Raw("\\Vendor\\Enum::CASE".to_owned()),
        );
        arguments.insert("plain".to_owned(), PhpValue::str("scalar"));
        arguments.insert(
            "With\\Object".to_owned(),
            in_v(obj("N\\C", vec![("a", PhpValue::Array(vec![])), ("b", PhpValue::Int(1))])),
        );
        let preferences =
            vec![("IfaceA".to_owned(), "ImplA".to_owned()), ("IfaceB".to_owned(), "ImplB".to_owned())];
        let instance_types: Vec<(String, String)> = Vec::new(); // empty section
        let non_lazy = vec!["Cls\\One".to_owned(), "Cls\\Two".to_owned()];

        let pairs = |entries: &[(String, String)]| {
            PhpValue::Array(
                entries
                    .iter()
                    .map(|(k, v)| (PhpKey::str(k.clone()), PhpValue::str(v.clone())))
                    .collect(),
            )
        };
        let wrapper = PhpValue::Array(vec![
            (
                PhpKey::str("arguments"),
                PhpValue::Array(
                    arguments.iter().map(|(k, v)| (PhpKey::str(k.clone()), v.clone())).collect(),
                ),
            ),
            (PhpKey::str("preferences"), pairs(&preferences)),
            (PhpKey::str("instanceTypes"), pairs(&instance_types)),
            (
                PhpKey::str("nonLazyTypes"),
                PhpValue::Array(
                    non_lazy.iter().map(|k| (PhpKey::str(k.clone()), PhpValue::Bool(true))).collect(),
                ),
            ),
        ]);
        assert_eq!(
            area_file_to_php(&arguments, &preferences, &instance_types, &non_lazy),
            to_php_file(&wrapper)
        );
    }

    #[test]
    fn empty_array_value() {
        let v = PhpValue::Array(vec![(PhpKey::str("k"), PhpValue::Array(vec![]))]);
        assert_eq!(
            to_php_file(&v),
            "<?php return array (\n  'k' => \n  array (\n  ),\n);"
        );
    }
}
