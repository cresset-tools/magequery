//! Static evaluation of PHP constant expressions — the values baked into
//! compiled metadata (`Foo::class` → the FQCN string, `self::X` chased to a
//! literal, arrays folded element by element). No PHP ever runs: anything
//! this module cannot evaluate is a hard [`EvalError`], never a guess.
//!
//! Two phases, matching how context flows:
//! - [`parse_const_expr`] turns raw expression text into a [`ConstExpr`] with
//!   every class name RESOLVED (needs the declaration's namespace + use map,
//!   which [`crate::ClassMeta`] carries).
//! - [`eval`] folds a [`ConstExpr`] to a [`ConstValue`], chasing class-const
//!   references through a caller-provided lookup (the whole-codebase index
//!   lives in the engine, not here).

use std::collections::BTreeMap;

use crate::scan::Cursor;

/// A parsed constant expression with resolved names.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ConstExpr {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// `key => value` items; key `None` = implicit index.
    Array(Vec<(Option<ConstExpr>, ConstExpr)>),
    /// `Foo\Bar::class` — already resolved to the FQCN string.
    ClassNameOf(String),
    /// `Foo::CONST`, `self::CONST`, `parent::CONST`, `static::CONST`.
    ClassConst { class: ClassRef, name: String },
    /// A bare global constant (`PHP_INT_MAX`, `SORT_ASC`, …).
    GlobalConst(String),
    /// Binary operation (`.`, `+`, `-`, `*`, `|`, `<<`, …).
    BinOp {
        op: BinOp,
        left: Box<ConstExpr>,
        right: Box<ConstExpr>,
    },
    /// Unary minus.
    Neg(Box<ConstExpr>),
    /// Anything this parser does not model (`new Foo()`, ternaries, calls).
    /// Evaluation fails loudly.
    Opaque(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClassRef {
    Fqcn(usize),
    SelfRef,
    ParentRef,
    StaticRef,
}

/// Interned FQCN storage would be overkill here; ClassRef::Fqcn indexes into
/// the expression's own class table.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ParsedExpr {
    pub expr: ConstExpr,
    /// FQCNs referenced via `ClassRef::Fqcn`.
    pub classes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BinOp {
    Concat,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Shl,
    Shr,
    BitOr,
    BitAnd,
    BitXor,
}

/// An evaluated PHP value. Array keys follow PHP's coercion rules
/// (int-like strings become ints, bools become 0/1, null becomes '').
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ConstValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Array(Vec<(ArrayKey, ConstValue)>),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum ArrayKey {
    Int(i64),
    Str(String),
}

#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct EvalError {
    pub message: String,
}

impl EvalError {
    fn new(msg: impl Into<String>) -> Self {
        EvalError { message: msg.into() }
    }
}

// ---- parsing ---------------------------------------------------------------

/// Parse raw constant-expression text. `namespace` and `uses` come from the
/// declaration the expression was written in ([`crate::ClassMeta::uses`]).
pub fn parse_const_expr(src: &str, namespace: &str, uses: &[(String, String)]) -> ParsedExpr {
    let use_map: BTreeMap<&str, &str> = uses
        .iter()
        .map(|(a, f)| (a.as_str(), f.as_str()))
        .collect();
    let mut p = ExprParser {
        cur: Cursor::new(src.as_bytes()),
        src,
        namespace,
        use_map,
        classes: Vec::new(),
    };
    let expr = p.parse_expr();
    p.cur.skip_insignificant();
    let expr = if p.cur.peek().is_some() {
        // Trailing junk — the whole thing is beyond this grammar.
        ConstExpr::Opaque(src.to_owned())
    } else {
        expr
    };
    ParsedExpr {
        expr,
        classes: p.classes,
    }
}

struct ExprParser<'a> {
    cur: Cursor<'a>,
    src: &'a str,
    namespace: &'a str,
    use_map: BTreeMap<&'a str, &'a str>,
    classes: Vec<String>,
}

impl<'a> ExprParser<'a> {
    fn opaque(&self) -> ConstExpr {
        ConstExpr::Opaque(self.src.to_owned())
    }

    fn intern(&mut self, fqcn: String) -> ClassRef {
        if let Some(i) = self.classes.iter().position(|c| c == &fqcn) {
            return ClassRef::Fqcn(i);
        }
        self.classes.push(fqcn);
        ClassRef::Fqcn(self.classes.len() - 1)
    }

    fn resolve(&self, name: &str) -> String {
        if let Some(rest) = name.strip_prefix('\\') {
            return rest.to_owned();
        }
        let (first, rest) = match name.split_once('\\') {
            Some((f, r)) => (f, Some(r)),
            None => (name, None),
        };
        if first.eq_ignore_ascii_case("namespace") {
            return match rest {
                Some(r) if !self.namespace.is_empty() => format!("{}\\{}", self.namespace, r),
                Some(r) => r.to_owned(),
                None => self.namespace.to_owned(),
            };
        }
        if let Some(mapped) = self.use_map.get(first.to_ascii_lowercase().as_str()) {
            return match rest {
                Some(r) => format!("{mapped}\\{r}"),
                None => (*mapped).to_owned(),
            };
        }
        if self.namespace.is_empty() {
            name.to_owned()
        } else {
            format!("{}\\{}", self.namespace, name)
        }
    }

    /// Precedence-climbing over the small operator set. PHP 8 precedence:
    /// `* / %`  >  `+ -`  >  `<< >>`  >  `.`  >  `&`  >  `^`  >  `|`.
    fn parse_expr(&mut self) -> ConstExpr {
        self.parse_binary(0)
    }

    fn parse_binary(&mut self, min_prec: u8) -> ConstExpr {
        let mut left = self.parse_unary();
        loop {
            self.cur.skip_insignificant();
            let (op, prec) = match self.cur.peek() {
                Some(b'*') => (BinOp::Mul, 60),
                Some(b'/') => (BinOp::Div, 60),
                Some(b'%') => (BinOp::Mod, 60),
                Some(b'+') => (BinOp::Add, 50),
                Some(b'-') => (BinOp::Sub, 50),
                Some(b'<') if self.cur.peek_at(1) == Some(b'<') => (BinOp::Shl, 40),
                Some(b'>') if self.cur.peek_at(1) == Some(b'>') => (BinOp::Shr, 40),
                Some(b'.') => (BinOp::Concat, 30),
                Some(b'&') => (BinOp::BitAnd, 25),
                Some(b'^') => (BinOp::BitXor, 22),
                Some(b'|') => (BinOp::BitOr, 20),
                _ => return left,
            };
            if prec < min_prec {
                return left;
            }
            match op {
                BinOp::Shl | BinOp::Shr => self.cur.pos += 2,
                _ => self.cur.bump(),
            }
            let right = self.parse_binary(prec + 1);
            left = ConstExpr::BinOp {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
    }

    fn parse_unary(&mut self) -> ConstExpr {
        self.cur.skip_insignificant();
        match self.cur.peek() {
            Some(b'-') => {
                self.cur.bump();
                ConstExpr::Neg(Box::new(self.parse_unary()))
            }
            Some(b'+') => {
                self.cur.bump();
                self.parse_unary()
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> ConstExpr {
        self.cur.skip_insignificant();
        match self.cur.peek() {
            Some(b'(') => {
                self.cur.bump();
                let inner = self.parse_expr();
                self.cur.skip_insignificant();
                if self.cur.peek() == Some(b')') {
                    self.cur.bump();
                    inner
                } else {
                    self.opaque()
                }
            }
            Some(b'[') => {
                self.cur.bump();
                self.parse_array(b']')
            }
            Some(b'\'' | b'"') => self.parse_string(),
            Some(b) if b.is_ascii_digit() => self.parse_number(),
            Some(b'\\') => self.parse_name(),
            Some(_) if self.cur.at_ident_start() => self.parse_name(),
            _ => self.opaque(),
        }
    }

    fn parse_array(&mut self, close: u8) -> ConstExpr {
        let mut items = Vec::new();
        loop {
            self.cur.skip_insignificant();
            match self.cur.peek() {
                Some(b) if b == close => {
                    self.cur.bump();
                    return ConstExpr::Array(items);
                }
                Some(b',') => {
                    self.cur.bump();
                }
                None => return self.opaque(),
                _ => {
                    if self.cur.starts_with(b"...") {
                        return self.opaque(); // spread: beyond this grammar
                    }
                    let first = self.parse_expr();
                    self.cur.skip_insignificant();
                    if self.cur.starts_with(b"=>") {
                        self.cur.pos += 2;
                        let value = self.parse_expr();
                        items.push((Some(first), value));
                    } else {
                        items.push((None, first));
                    }
                }
            }
        }
    }

    fn parse_string(&mut self) -> ConstExpr {
        let quote = self.cur.peek().unwrap();
        self.cur.bump();
        let mut out = String::new();
        while let Some(b) = self.cur.peek() {
            if b == quote {
                self.cur.bump();
                return ConstExpr::Str(out);
            }
            if b == b'\\' {
                self.cur.bump();
                let Some(esc) = self.cur.peek() else { break };
                self.cur.bump();
                if quote == b'\'' {
                    // Single quotes: only \\ and \' are escapes.
                    match esc {
                        b'\\' | b'\'' => out.push(esc as char),
                        other => {
                            out.push('\\');
                            out.push(other as char);
                        }
                    }
                } else {
                    match esc {
                        b'n' => out.push('\n'),
                        b't' => out.push('\t'),
                        b'r' => out.push('\r'),
                        b'v' => out.push('\u{B}'),
                        b'f' => out.push('\u{C}'),
                        b'e' => out.push('\u{1B}'),
                        b'0' => out.push('\0'),
                        b'\\' | b'"' | b'$' => out.push(esc as char),
                        // \x, \u, octal: rare in metadata-relevant code.
                        _ => return self.opaque(),
                    }
                }
                continue;
            }
            // Interpolation in a const expr means it wasn't const after all.
            if quote == b'"' && (b == b'$' || b == b'{') {
                return self.opaque();
            }
            let start = self.cur.pos;
            self.cur.bump();
            while matches!(self.cur.peek(), Some(nb) if nb != quote && nb != b'\\' && !(quote == b'"' && (nb == b'$' || nb == b'{')))
            {
                self.cur.bump();
            }
            out.push_str(&String::from_utf8_lossy(
                &self.cur.src[start..self.cur.pos],
            ));
        }
        self.opaque()
    }

    fn parse_number(&mut self) -> ConstExpr {
        let start = self.cur.pos;
        let src = self.cur.src;
        let mut is_float = false;
        if src[start..].starts_with(b"0x") || src[start..].starts_with(b"0X") {
            self.cur.pos += 2;
            while matches!(self.cur.peek(), Some(b) if b.is_ascii_hexdigit() || b == b'_') {
                self.cur.bump();
            }
            let text: String = String::from_utf8_lossy(&src[start + 2..self.cur.pos]).replace('_', "");
            return match i64::from_str_radix(&text, 16) {
                Ok(v) => ConstExpr::Int(v),
                Err(_) => self.opaque(),
            };
        }
        while matches!(self.cur.peek(), Some(b) if b.is_ascii_digit() || b == b'_') {
            self.cur.bump();
        }
        if self.cur.peek() == Some(b'.') && matches!(self.cur.peek_at(1), Some(b) if b.is_ascii_digit())
        {
            is_float = true;
            self.cur.bump();
            while matches!(self.cur.peek(), Some(b) if b.is_ascii_digit() || b == b'_') {
                self.cur.bump();
            }
        }
        if matches!(self.cur.peek(), Some(b'e' | b'E')) {
            is_float = true;
            self.cur.bump();
            if matches!(self.cur.peek(), Some(b'+' | b'-')) {
                self.cur.bump();
            }
            while matches!(self.cur.peek(), Some(b) if b.is_ascii_digit()) {
                self.cur.bump();
            }
        }
        let text: String = String::from_utf8_lossy(&src[start..self.cur.pos]).replace('_', "");
        if is_float {
            match text.parse::<f64>() {
                Ok(v) => ConstExpr::Float(v),
                Err(_) => self.opaque(),
            }
        } else {
            match text.parse::<i64>() {
                Ok(v) => ConstExpr::Int(v),
                Err(_) => self.opaque(),
            }
        }
    }

    fn parse_name(&mut self) -> ConstExpr {
        let mut name = String::new();
        if self.cur.peek() == Some(b'\\') {
            self.cur.bump();
            name.push('\\');
        }
        loop {
            match self.cur.read_ident() {
                Some(seg) => name.push_str(seg),
                None => return self.opaque(),
            }
            if self.cur.peek() == Some(b'\\') {
                self.cur.bump();
                name.push('\\');
            } else {
                break;
            }
        }
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            "null" => return ConstExpr::Null,
            "true" => return ConstExpr::Bool(true),
            "false" => return ConstExpr::Bool(false),
            _ => {}
        }
        self.cur.skip_insignificant();
        if self.cur.starts_with(b"::") {
            self.cur.pos += 2;
            self.cur.skip_insignificant();
            let class = match lower.as_str() {
                "self" => ClassRef::SelfRef,
                "parent" => ClassRef::ParentRef,
                "static" => ClassRef::StaticRef,
                _ => {
                    let fqcn = self.resolve(&name);
                    self.intern(fqcn)
                }
            };
            let Some(member) = self.cur.read_ident() else {
                return self.opaque();
            };
            if member.eq_ignore_ascii_case("class") {
                return match class {
                    ClassRef::Fqcn(i) => ConstExpr::ClassNameOf(self.classes[i].clone()),
                    // self::class etc. need the declaring class; keep as a
                    // ClassConst so eval can resolve with context.
                    other => ConstExpr::ClassConst {
                        class: other,
                        name: "class".to_owned(),
                    },
                };
            }
            return ConstExpr::ClassConst {
                class,
                name: member.to_owned(),
            };
        }
        // A bare name that survived the keyword check: global constant.
        // PHP resolves unqualified constants namespace-relative then global;
        // in practice metadata-relevant ones are the global core constants.
        ConstExpr::GlobalConst(name)
    }
}

// ---- evaluation -------------------------------------------------------------

/// Resolves class-const references during evaluation. Implemented by the
/// engine over the parsed corpus.
pub trait ConstLookup {
    /// The raw expression text + declaring context of `Class::NAME`, or None.
    fn class_const(&self, class: &str, name: &str) -> Option<ParsedExpr>;
    /// The parent class of `class`, if any.
    fn parent_of(&self, class: &str) -> Option<String>;
}

pub struct EvalCtx<'a> {
    pub lookup: &'a dyn ConstLookup,
    /// The class the expression is written in (`self::`).
    pub self_class: Option<&'a str>,
    /// Recursion guard.
    depth: usize,
}

impl<'a> EvalCtx<'a> {
    pub fn new(lookup: &'a dyn ConstLookup, self_class: Option<&'a str>) -> Self {
        EvalCtx {
            lookup,
            self_class,
            depth: 0,
        }
    }
}

/// A small table of the PHP core constants that show up in real di config
/// and constructor defaults. Extend as the oracle demands.
fn global_const(name: &str) -> Option<ConstValue> {
    Some(match name {
        "PHP_INT_MAX" => ConstValue::Int(i64::MAX),
        "PHP_INT_MIN" => ConstValue::Int(i64::MIN),
        "PHP_INT_SIZE" => ConstValue::Int(8),
        "PHP_EOL" => ConstValue::Str("\n".to_owned()),
        "DIRECTORY_SEPARATOR" => ConstValue::Str("/".to_owned()),
        "SORT_ASC" => ConstValue::Int(4),
        "SORT_DESC" => ConstValue::Int(3),
        "SORT_REGULAR" => ConstValue::Int(0),
        "SORT_NUMERIC" => ConstValue::Int(1),
        "SORT_STRING" => ConstValue::Int(2),
        "SORT_FLAG_CASE" => ConstValue::Int(8),
        "JSON_UNESCAPED_SLASHES" => ConstValue::Int(64),
        "JSON_UNESCAPED_UNICODE" => ConstValue::Int(256),
        "JSON_PRETTY_PRINT" => ConstValue::Int(128),
        "JSON_HEX_TAG" => ConstValue::Int(1),
        "JSON_HEX_AMP" => ConstValue::Int(2),
        "JSON_HEX_APOS" => ConstValue::Int(4),
        "JSON_HEX_QUOT" => ConstValue::Int(8),
        "E_ALL" => ConstValue::Int(32767),
        "E_ERROR" => ConstValue::Int(1),
        "E_WARNING" => ConstValue::Int(2),
        "E_NOTICE" => ConstValue::Int(8),
        "E_DEPRECATED" => ConstValue::Int(8192),
        "E_USER_DEPRECATED" => ConstValue::Int(16384),
        "PHP_ROUND_HALF_UP" => ConstValue::Int(1),
        "M_PI" => ConstValue::Float(std::f64::consts::PI),
        _ => return None,
    })
}

pub fn eval(parsed: &ParsedExpr, ctx: &EvalCtx) -> Result<ConstValue, EvalError> {
    eval_expr(&parsed.expr, &parsed.classes, ctx)
}

fn eval_expr(
    expr: &ConstExpr,
    classes: &[String],
    ctx: &EvalCtx,
) -> Result<ConstValue, EvalError> {
    if ctx.depth > 32 {
        return Err(EvalError::new("constant reference cycle"));
    }
    match expr {
        ConstExpr::Null => Ok(ConstValue::Null),
        ConstExpr::Bool(b) => Ok(ConstValue::Bool(*b)),
        ConstExpr::Int(i) => Ok(ConstValue::Int(*i)),
        ConstExpr::Float(f) => Ok(ConstValue::Float(*f)),
        ConstExpr::Str(s) => Ok(ConstValue::Str(s.clone())),
        ConstExpr::ClassNameOf(fqcn) => Ok(ConstValue::Str(fqcn.clone())),
        ConstExpr::GlobalConst(name) => {
            global_const(name).ok_or_else(|| EvalError::new(format!("unknown constant {name}")))
        }
        ConstExpr::Neg(inner) => match eval_expr(inner, classes, ctx)? {
            ConstValue::Int(i) => Ok(ConstValue::Int(-i)),
            ConstValue::Float(f) => Ok(ConstValue::Float(-f)),
            _ => Err(EvalError::new("negation of non-number")),
        },
        ConstExpr::Array(items) => {
            let mut out: Vec<(ArrayKey, ConstValue)> = Vec::new();
            let mut next_index: i64 = 0;
            for (key, value) in items {
                let value = eval_expr(value, classes, ctx)?;
                let key = match key {
                    None => {
                        let k = ArrayKey::Int(next_index);
                        next_index += 1;
                        k
                    }
                    Some(k) => match eval_expr(k, classes, ctx)? {
                        ConstValue::Int(i) => {
                            next_index = next_index.max(i + 1);
                            ArrayKey::Int(i)
                        }
                        ConstValue::Str(s) => {
                            // PHP coerces integer-like string keys to ints.
                            if let Ok(i) = s.parse::<i64>() {
                                if i.to_string() == s {
                                    next_index = next_index.max(i + 1);
                                    ArrayKey::Int(i)
                                } else {
                                    ArrayKey::Str(s)
                                }
                            } else {
                                ArrayKey::Str(s)
                            }
                        }
                        ConstValue::Bool(b) => {
                            let i = b as i64;
                            next_index = next_index.max(i + 1);
                            ArrayKey::Int(i)
                        }
                        ConstValue::Null => ArrayKey::Str(String::new()),
                        ConstValue::Float(f) => {
                            let i = f as i64;
                            next_index = next_index.max(i + 1);
                            ArrayKey::Int(i)
                        }
                        ConstValue::Array(_) => {
                            return Err(EvalError::new("array used as array key"))
                        }
                    },
                };
                // PHP semantics: a repeated key overwrites in place.
                if let Some(slot) = out.iter_mut().find(|(k, _)| *k == key) {
                    slot.1 = value;
                } else {
                    out.push((key, value));
                }
            }
            Ok(ConstValue::Array(out))
        }
        ConstExpr::ClassConst { class, name } => {
            let class_name = match class {
                ClassRef::Fqcn(i) => classes
                    .get(*i)
                    .cloned()
                    .ok_or_else(|| EvalError::new("bad class ref"))?,
                ClassRef::SelfRef | ClassRef::StaticRef => ctx
                    .self_class
                    .map(str::to_owned)
                    .ok_or_else(|| EvalError::new("self:: outside class context"))?,
                ClassRef::ParentRef => {
                    let self_class = ctx
                        .self_class
                        .ok_or_else(|| EvalError::new("parent:: outside class context"))?;
                    ctx.lookup
                        .parent_of(self_class)
                        .ok_or_else(|| EvalError::new("parent:: with no parent"))?
                }
            };
            if name == "class" {
                return Ok(ConstValue::Str(class_name));
            }
            // Chase the constant up the hierarchy (constants inherit).
            let mut current = Some(class_name.clone());
            while let Some(c) = current {
                if let Some(parsed) = ctx.lookup.class_const(&c, name) {
                    let inner_ctx = EvalCtx {
                        lookup: ctx.lookup,
                        self_class: Some(&c),
                        depth: ctx.depth + 1,
                    };
                    return eval_expr(&parsed.expr, &parsed.classes, &inner_ctx);
                }
                current = ctx.lookup.parent_of(&c);
            }
            Err(EvalError::new(format!(
                "constant {class_name}::{name} not found"
            )))
        }
        ConstExpr::BinOp { op, left, right } => {
            let l = eval_expr(left, classes, ctx)?;
            let r = eval_expr(right, classes, ctx)?;
            eval_binop(*op, l, r)
        }
        ConstExpr::Opaque(text) => Err(EvalError::new(format!(
            "expression not statically evaluable: {text}"
        ))),
    }
}

fn eval_binop(op: BinOp, l: ConstValue, r: ConstValue) -> Result<ConstValue, EvalError> {
    use ConstValue::*;
    if op == BinOp::Concat {
        let ls = to_php_string(&l)?;
        let rs = to_php_string(&r)?;
        return Ok(Str(ls + &rs));
    }
    match (l, r) {
        (Int(a), Int(b)) => Ok(match op {
            BinOp::Add => Int(a.wrapping_add(b)),
            BinOp::Sub => Int(a.wrapping_sub(b)),
            BinOp::Mul => Int(a.wrapping_mul(b)),
            BinOp::Div => {
                if b != 0 && a % b == 0 {
                    Int(a / b)
                } else if b != 0 {
                    Float(a as f64 / b as f64)
                } else {
                    return Err(EvalError::new("division by zero"));
                }
            }
            BinOp::Mod => {
                if b != 0 {
                    Int(a % b)
                } else {
                    return Err(EvalError::new("modulo by zero"));
                }
            }
            BinOp::Shl => Int(a.wrapping_shl(b as u32)),
            BinOp::Shr => Int(a.wrapping_shr(b as u32)),
            BinOp::BitOr => Int(a | b),
            BinOp::BitAnd => Int(a & b),
            BinOp::BitXor => Int(a ^ b),
            BinOp::Concat => unreachable!(),
        }),
        (Float(a), Float(b)) => eval_float_op(op, a, b),
        (Int(a), Float(b)) => eval_float_op(op, a as f64, b),
        (Float(a), Int(b)) => eval_float_op(op, a, b as f64),
        _ => Err(EvalError::new("arithmetic on non-numbers")),
    }
}

fn eval_float_op(op: BinOp, a: f64, b: f64) -> Result<ConstValue, EvalError> {
    use ConstValue::Float;
    Ok(match op {
        BinOp::Add => Float(a + b),
        BinOp::Sub => Float(a - b),
        BinOp::Mul => Float(a * b),
        BinOp::Div => Float(a / b),
        _ => return Err(EvalError::new("integer op on floats")),
    })
}

fn to_php_string(v: &ConstValue) -> Result<String, EvalError> {
    Ok(match v {
        ConstValue::Str(s) => s.clone(),
        ConstValue::Int(i) => i.to_string(),
        ConstValue::Bool(true) => "1".to_owned(),
        ConstValue::Bool(false) => String::new(),
        ConstValue::Null => String::new(),
        ConstValue::Float(f) => format!("{f}"),
        ConstValue::Array(_) => return Err(EvalError::new("array to string conversion")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoLookup;
    impl ConstLookup for NoLookup {
        fn class_const(&self, _: &str, _: &str) -> Option<ParsedExpr> {
            None
        }
        fn parent_of(&self, _: &str) -> Option<String> {
            None
        }
    }

    fn ev(src: &str) -> ConstValue {
        let parsed = parse_const_expr(src, "App", &[("dom".into(), "Magento\\Framework\\Config\\Dom".into())]);
        eval(&parsed, &EvalCtx::new(&NoLookup, None)).unwrap()
    }

    #[test]
    fn literals() {
        assert_eq!(ev("null"), ConstValue::Null);
        assert_eq!(ev("TRUE"), ConstValue::Bool(true));
        assert_eq!(ev("42"), ConstValue::Int(42));
        assert_eq!(ev("-7"), ConstValue::Int(-7));
        assert_eq!(ev("0x1F"), ConstValue::Int(31));
        assert_eq!(ev("1_000"), ConstValue::Int(1000));
        assert_eq!(ev("'it\\'s'"), ConstValue::Str("it's".into()));
        assert_eq!(ev("\"a\\nb\""), ConstValue::Str("a\nb".into()));
    }

    #[test]
    fn class_name_resolution() {
        // ::class through the use map, absolute, and namespace-relative.
        assert_eq!(
            ev("Dom::class"),
            ConstValue::Str("Magento\\Framework\\Config\\Dom".into())
        );
        assert_eq!(ev("\\Foo\\Bar::class"), ConstValue::Str("Foo\\Bar".into()));
        assert_eq!(ev("Local::class"), ConstValue::Str("App\\Local".into()));
    }

    #[test]
    fn arrays_with_php_key_semantics() {
        assert_eq!(
            ev("['a' => 1, 2 => 'x', 'b']"),
            ConstValue::Array(vec![
                (ArrayKey::Str("a".into()), ConstValue::Int(1)),
                (ArrayKey::Int(2), ConstValue::Str("x".into())),
                (ArrayKey::Int(3), ConstValue::Str("b".into())),
            ])
        );
        // Int-like string keys coerce; repeated keys overwrite.
        assert_eq!(
            ev("['5' => 'a', 5 => 'b']"),
            ConstValue::Array(vec![(ArrayKey::Int(5), ConstValue::Str("b".into()))])
        );
    }

    #[test]
    fn operators() {
        assert_eq!(ev("1 + 2 * 3"), ConstValue::Int(7));
        assert_eq!(ev("1 << 4"), ConstValue::Int(16));
        assert_eq!(ev("'a' . 'b' . 1"), ConstValue::Str("ab1".into()));
        assert_eq!(ev("PHP_INT_MAX"), ConstValue::Int(i64::MAX));
        assert_eq!(ev("6 | 1"), ConstValue::Int(7));
    }

    #[test]
    fn const_chasing_through_lookup() {
        struct Table;
        impl ConstLookup for Table {
            fn class_const(&self, class: &str, name: &str) -> Option<ParsedExpr> {
                match (class, name) {
                    ("A\\B", "ENTITY") => Some(parse_const_expr("'catalog_' . self::SUFFIX", "A", &[])),
                    ("A\\B", "SUFFIX") => Some(parse_const_expr("'product'", "A", &[])),
                    _ => None,
                }
            }
            fn parent_of(&self, class: &str) -> Option<String> {
                (class == "A\\C").then(|| "A\\B".to_owned())
            }
        }
        // Direct, self-referencing, and inherited (A\C has no own ENTITY).
        let parsed = parse_const_expr("\\A\\B::ENTITY", "X", &[]);
        assert_eq!(
            eval(&parsed, &EvalCtx::new(&Table, None)).unwrap(),
            ConstValue::Str("catalog_product".into())
        );
        let parsed = parse_const_expr("\\A\\C::ENTITY", "X", &[]);
        assert_eq!(
            eval(&parsed, &EvalCtx::new(&Table, None)).unwrap(),
            ConstValue::Str("catalog_product".into())
        );
    }

    #[test]
    fn unevaluable_is_a_hard_error() {
        let parsed = parse_const_expr("new Foo()", "X", &[]);
        assert!(matches!(parsed.expr, ConstExpr::Opaque(_)));
        assert!(eval(&parsed, &EvalCtx::new(&NoLookup, None)).is_err());
        let parsed = parse_const_expr("UNKNOWN_CONST", "X", &[]);
        assert!(eval(&parsed, &EvalCtx::new(&NoLookup, None)).is_err());
    }
}
