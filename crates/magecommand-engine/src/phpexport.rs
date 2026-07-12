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
    /// Entries in output order — the caller owns ordering (ksort or
    /// insertion order, whatever the Magento generator being mirrored does).
    Array(Vec<(PhpKey, PhpValue)>),
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

fn export(value: &PhpValue, indent: usize, out: &mut String) {
    match value {
        PhpValue::Str(s) => {
            out.push('\'');
            for ch in s.chars() {
                if ch == '\\' || ch == '\'' {
                    out.push('\\');
                }
                out.push(ch);
            }
            out.push('\'');
        }
        PhpValue::Int(i) => out.push_str(&i.to_string()),
        PhpValue::Float(f) => {
            if f.fract() == 0.0 && f.is_finite() && f.abs() < 1e15 {
                out.push_str(&format!("{f:.1}"));
            } else {
                out.push_str(&format!("{f}"));
            }
        }
        PhpValue::Bool(true) => out.push_str("true"),
        PhpValue::Bool(false) => out.push_str("false"),
        PhpValue::Null => out.push_str("NULL"),
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
                if matches!(val, PhpValue::Array(_)) {
                    // var_export puts nested arrays on their own line at the
                    // key's indent — after a trailing space on the key line.
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
    }
}

#[cfg(test)]
mod tests {
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
    fn quotes_are_escaped() {
        let v = PhpValue::Array(vec![(PhpKey::str("k"), PhpValue::str("it's"))]);
        assert_eq!(
            to_php_file(&v),
            "<?php return array (\n  'k' => 'it\\'s',\n);"
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
