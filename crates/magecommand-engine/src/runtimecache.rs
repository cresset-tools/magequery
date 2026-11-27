//! Cache entries a developer-mode PHP boot would build after a cache clean,
//! computed natively so `dev upgrade` can write them first.

use std::cell::RefCell;
use std::collections::HashSet;

use magecommand_php::constexpr::{ArrayKey, ConstValue};
use magecommand_php::constexpr::{eval, parse_const_expr, ConstLookup, EvalCtx, ParsedExpr};
use magequery_core::phparray::PhpValue;
use magequery_core::Magento;

use crate::arguments::DefsLookup;
use crate::definitions::Definitions;

/// The `GLOBAL__DICONFIG` payload (`ObjectManager\ConfigLoader::load('global')`).
/// Constants are evaluated from source, loading each class the evaluation
/// reaches (and its ancestors) into `defs` on first use: the class a di.xml
/// constant names, and any its value refers to in turn
/// (`CODE = Config::PREFIX . 'hosted_fields'`, `Config` unimported in the same
/// namespace).
pub fn global_di_config(magento: &Magento, defs: Definitions) -> Result<String, String> {
    let defs = RefCell::new(defs);
    let lookup = LoadingLookup { magento, defs: &defs, missing: RefCell::default() };
    let constant = |name: &str| -> Option<PhpValue> {
        // di.xml constants are written fully qualified; no use map applies.
        let parsed = parse_const_expr(name, "", &[]);
        eval(&parsed, &EvalCtx::new(&lookup, None)).ok().and_then(|v| to_php(&v))
    };
    magento.global_di_config(&constant).map_err(|e| e.to_string())
}

/// [`DefsLookup`] over definitions that load a class when it is first asked
/// about. A class that cannot be loaded is remembered, not looked for again.
struct LoadingLookup<'a> {
    magento: &'a Magento,
    defs: &'a RefCell<Definitions>,
    missing: RefCell<HashSet<String>>,
}

impl LoadingLookup<'_> {
    fn ensure(&self, class: &str) {
        let class = class.trim_start_matches('\\');
        if self.defs.borrow().get(class).is_some() || self.missing.borrow().contains(class) {
            return;
        }
        self.defs.borrow_mut().load_classes(self.magento, self.magento.root(), [class.to_owned()]);
        if self.defs.borrow().get(class).is_none() {
            self.missing.borrow_mut().insert(class.to_owned());
        }
    }
}

impl ConstLookup for LoadingLookup<'_> {
    fn class_const(&self, class: &str, name: &str) -> Option<ParsedExpr> {
        self.ensure(class);
        let defs = self.defs.borrow();
        let found = DefsLookup { defs: &defs }.class_const(class, name);
        found
    }

    fn parent_of(&self, class: &str) -> Option<String> {
        self.ensure(class);
        let defs = self.defs.borrow();
        let found = DefsLookup { defs: &defs }.parent_of(class);
        found
    }
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
