//! Cache entries a developer-mode PHP boot would build after a cache clean,
//! computed natively so `dev upgrade` can write them first.

use std::cell::RefCell;

use magecommand_php::constexpr::{ArrayKey, ConstValue};
use magecommand_php::constexpr::{eval, parse_const_expr, EvalCtx};
use magequery_core::phparray::PhpValue;
use magequery_core::Magento;

use crate::arguments::DefsLookup;
use crate::definitions::Definitions;

/// The `GLOBAL__DICONFIG` payload (`ObjectManager\ConfigLoader::load('global')`).
/// Constants are evaluated from source, loading each referenced class (and its
/// ancestors) into `defs` on first use.
pub fn global_di_config(magento: &Magento, defs: Definitions) -> Result<String, String> {
    let defs = RefCell::new(defs);
    let constant = |name: &str| -> Option<PhpValue> {
        if let Some((class, _)) = name.trim_start_matches('\\').rsplit_once("::")
            && defs.borrow().get(class).is_none()
        {
            defs.borrow_mut().load_classes(magento, magento.root(), [class.to_owned()]);
        }
        let defs = defs.borrow();
        // di.xml constants are written fully qualified; no use map applies.
        let parsed = parse_const_expr(name, "", &[]);
        let lookup = DefsLookup { defs: &defs };
        eval(&parsed, &EvalCtx::new(&lookup, None)).ok().and_then(|v| to_php(&v))
    };
    magento.global_di_config(&constant).map_err(|e| e.to_string())
}

fn to_php(v: &ConstValue) -> Option<PhpValue> {
    Some(match v {
        ConstValue::Null => PhpValue::Null,
        ConstValue::Bool(b) => PhpValue::Bool(*b),
        ConstValue::Int(i) => PhpValue::Int(*i),
        ConstValue::Float(f) => PhpValue::Float(*f),
        ConstValue::Str(s) => PhpValue::Str(s.clone()),
        ConstValue::Array(items) => PhpValue::Array(
            items
                .iter()
                .map(|(k, v)| {
                    let key = match k {
                        ArrayKey::Int(i) => PhpValue::Int(*i),
                        ArrayKey::Str(s) => PhpValue::Str(s.clone()),
                        _ => return None,
                    };
                    Some((key, to_php(v)?))
                })
                .collect::<Option<Vec<_>>>()?,
        ),
        _ => return None,
    })
}
