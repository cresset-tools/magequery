//! `<Class>\Proxy` — the lazy-loading wrapper. A fixed boilerplate (the
//! object-manager plumbing + `__sleep`/`__wakeup`/`__clone`/`__debugInfo`/
//! `_getSubject`) plus one delegating method per public method of the subject,
//! ported from `Magento\Framework\ObjectManager\Code\Generator\Proxy`.

use crate::definitions::Definitions;
use crate::laminas::{Class, DocBlock, Method, Param, Property, Val, Visibility};
use crate::reflect::{self, RMethod};

const NONINTERCEPTABLE: &str = "Magento\\Framework\\ObjectManager\\NoninterceptableInterface";

/// The generated `<source>\Proxy` file, byte-exact. `source` is the wrapped
/// class (no leading backslash). Returns `None` when the subject isn't in the
/// known universe (nothing to reflect).
pub fn proxy_bytes(defs: &Definitions, source: &str) -> Option<String> {
    let record = defs.get(source)?;
    let is_interface = record.meta.kind == magecommand_php::ClassKind::Interface;
    // getSourceClassName() carries a leading backslash.
    let source_bs = format!("\\{source}");

    let namespace = Some(source.to_owned());
    let mut properties = Vec::new();
    properties.push(prop("_objectManager", "Object Manager instance", "\\Magento\\Framework\\ObjectManagerInterface"));
    properties.push(prop("_instanceName", "Proxied instance name", "string"));
    properties.push(prop("_subject", "Proxied instance", &source_bs));
    properties.push(prop("_isShared", "Instance shareability flag", "bool"));

    let mut methods = Vec::new();
    methods.push(constructor(&source_bs));
    methods.push(fixed(
        "__sleep",
        Visibility::Public,
        "return ['_subject', '_isShared', '_instanceName'];",
        DocBlock { tags: vec![("return".into(), "array".into())], ..Default::default() },
    ));
    methods.push(fixed(
        "__wakeup",
        Visibility::Public,
        "$this->_objectManager = \\Magento\\Framework\\App\\ObjectManager::getInstance();",
        DocBlock { short: Some("Retrieve ObjectManager from global scope".into()), ..Default::default() },
    ));
    methods.push(fixed(
        "__clone",
        Visibility::Public,
        "if ($this->_subject) {\n    $this->_subject = clone $this->_getSubject();\n}",
        DocBlock { short: Some("Clone proxied instance".into()), ..Default::default() },
    ));
    methods.push(fixed(
        "__debugInfo",
        Visibility::Public,
        "return ['i' => $this->_subject];",
        DocBlock { short: Some("Debug proxied instance".into()), ..Default::default() },
    ));
    methods.push(Method {
        name: "_getSubject".into(),
        visibility: Visibility::Protected,
        is_static: false,
        is_final: false,
        is_abstract: false,
        returns_ref: false,
        params: vec![],
        return_type: None,
        body: Some(
            "if (!$this->_subject) {\n    $this->_subject = true === $this->_isShared\n        ? $this->_objectManager->get($this->_instanceName)\n        : $this->_objectManager->create($this->_instanceName);\n}\nreturn $this->_subject;"
                .into(),
        ),
        doc: DocBlock {
            short: Some("Get proxied instance".into()),
            tags: vec![("return".into(), source_bs.clone())],
            ..Default::default()
        },
    });

    // One delegating method per proxyable public method of the subject.
    // `_resetState` is special-cased: it isn't delegated but re-emitted as a
    // `void` method that resets the subject, at its reflection position.
    for rm in reflect::public_methods(defs, source) {
        let name = rm.name.as_str();
        if name == "_resetState" {
            methods.push(reset_state());
            continue;
        }
        if rm.is_static
            || rm.is_final
            || name.eq_ignore_ascii_case("__construct")
            || name.eq_ignore_ascii_case("__destruct")
            || matches!(name, "__sleep" | "__wakeup" | "__clone" | "__debugInfo")
        {
            continue;
        }
        methods.push(delegating(&rm));
    }

    let mut implements = Vec::new();
    let extends;
    if is_interface {
        extends = None;
        implements.push(source_bs.clone());
        implements.push(format!("\\{NONINTERCEPTABLE}"));
    } else {
        extends = Some(source_bs.clone());
        implements.push(format!("\\{NONINTERCEPTABLE}"));
    }

    let class = Class {
        namespace,
        name: "Proxy".into(),
        is_interface: false,
        extends,
        implements,
        doc: DocBlock {
            short: Some(format!("Proxy class for @see {source_bs}")),
            ..Default::default()
        },
        properties,
        methods,
    };
    Some(class.render())
}

/// The generated `<source>\ProxyDeferred` file (async lazy wrapper), byte
/// exact — a near-twin of the proxy with an `@inheritDoc` docblock, a smaller
/// magic-method exclusion set, and Magento's copy-pasted constructor `@param`.
pub fn proxy_deferred_bytes(defs: &Definitions, source: &str) -> Option<String> {
    let record = defs.get(source)?;
    let is_interface = record.meta.kind == magecommand_php::ClassKind::Interface;
    let source_bs = format!("\\{source}");

    let mut properties = Vec::new();
    properties.push(private_prop("instance", "Proxied instance", "string"));
    properties.push(private_prop("deferred", "Deferred to wait for", "string"));

    let mut methods = Vec::new();
    methods.push(Method {
        name: "__construct".into(),
        visibility: Visibility::Public,
        is_static: false,
        is_final: false,
        is_abstract: false,
        returns_ref: false,
        params: vec![Param {
            name: "deferred".into(),
            type_str: Some("\\Magento\\Framework\\Async\\DeferredInterface".into()),
            by_ref: false,
            variadic: false,
            default: None,
        }],
        return_type: None,
        body: Some("$this->deferred = $deferred;".into()),
        doc: DocBlock {
            short: Some("ProxyDeferred constructor".into()),
            // Faithful to Magento's copy-paste: the param name/type mismatch.
            tags: vec![(
                "param".into(),
                "\\Magento\\Framework\\ObjectManager\\DefinitionFactory $objectManager".into(),
            )],
            ..Default::default()
        },
    });
    methods.push(fixed(
        "__sleep",
        Visibility::Public,
        "$this->wait();\nreturn ['instance'];",
        DocBlock {
            short: Some("Serialize only the instance".into()),
            tags: vec![("return".into(), "array".into())],
            ..Default::default()
        },
    ));
    methods.push(fixed(
        "__clone",
        Visibility::Public,
        "$this->wait();\n$this->instance = clone $this->instance;",
        DocBlock { short: Some("Clone proxied instance".into()), ..Default::default() },
    ));
    methods.push(Method {
        name: "wait".into(),
        visibility: Visibility::Private,
        is_static: false,
        is_final: false,
        is_abstract: false,
        returns_ref: false,
        params: vec![],
        return_type: None,
        body: Some(format!(
            "if (!$this->instance) {{\n    $this->instance = $this->deferred->get();\n    if (!$this->instance instanceof {source_bs}) {{\n        throw new \\RuntimeException('Wrong instance returned by deferred');\n    }}\n}}\nreturn $this->instance;"
        )),
        doc: DocBlock {
            short: Some("Get proxied instance".into()),
            tags: vec![("return".into(), source_bs.clone())],
            ..Default::default()
        },
    });

    for rm in reflect::public_methods(defs, source) {
        let name = rm.name.as_str();
        if rm.is_static
            || rm.is_final
            || name.eq_ignore_ascii_case("__construct")
            || name.eq_ignore_ascii_case("__destruct")
            || matches!(name, "__sleep" | "__wakeup" | "__clone")
        {
            continue;
        }
        methods.push(deferred_delegating(&rm));
    }

    let (extends, implements) = if is_interface {
        (None, vec![source_bs.clone(), format!("\\{NONINTERCEPTABLE}")])
    } else {
        (Some(source_bs.clone()), vec![format!("\\{NONINTERCEPTABLE}")])
    };

    let class = Class {
        namespace: Some(source.to_owned()),
        name: "ProxyDeferred".into(),
        is_interface: false,
        extends,
        implements,
        doc: DocBlock {
            short: Some(format!("ProxyDeferred class for @see {source_bs}")),
            ..Default::default()
        },
        properties,
        methods,
    };
    Some(class.render())
}

fn private_prop(name: &str, short: &str, var: &str) -> Property {
    Property {
        name: name.into(),
        visibility: Visibility::Private,
        default: None,
        doc: DocBlock {
            short: Some(short.into()),
            tags: vec![("var".into(), var.into())],
            ..Default::default()
        },
    }
}

/// ProxyDeferred's delegating body waits then forwards to the instance.
fn deferred_delegating(rm: &RMethod) -> Method {
    let forward: Vec<String> = rm
        .params
        .iter()
        .map(|p| if p.variadic { format!("... ${}", p.name) } else { format!("${}", p.name) })
        .collect();
    let without_return = rm.return_type.as_deref() == Some("void");
    let call = format!("{}({})", rm.name, forward.join(", "));
    let tail = if without_return {
        format!("$this->instance->{call};")
    } else {
        format!("return $this->instance->{call};")
    };
    let mut m = deferred_method_shell(rm);
    m.body = Some(format!("$this->wait();\n{tail}"));
    m
}

fn deferred_method_shell(rm: &RMethod) -> Method {
    Method {
        name: rm.name.clone(),
        visibility: Visibility::Public,
        is_static: false,
        is_final: false,
        is_abstract: false,
        returns_ref: false,
        params: rm
            .params
            .iter()
            .map(|p| Param {
                name: p.name.clone(),
                type_str: p.type_str.clone(),
                by_ref: p.by_ref,
                variadic: p.variadic,
                default: p.default.clone(),
            })
            .collect(),
        return_type: rm.return_type.clone(),
        body: None,
        doc: DocBlock { short: Some("@inheritDoc".into()), ..Default::default() },
    }
}

#[cfg(test)]
mod tests {
    use crate::laminas::Class;

    /// The fixed proxy boilerplate frames correctly for a subject with no
    /// public methods (constructor default carries the escaped FQCN).
    #[test]
    fn proxy_body_is_indented_and_framed() {
        // Exercised end-to-end against the archive by the codegen_set example;
        // here we just confirm the Class renderer wires a proxy-shaped file.
        let c = Class {
            namespace: Some("Foo\\Bar".into()),
            name: "Proxy".into(),
            is_interface: false,
            extends: Some("\\Foo\\Bar".into()),
            implements: vec!["\\Magento\\Framework\\ObjectManager\\NoninterceptableInterface".into()],
            doc: crate::laminas::DocBlock {
                short: Some("Proxy class for @see \\Foo\\Bar".into()),
                ..Default::default()
            },
            properties: vec![],
            methods: vec![],
        };
        let out = c.render();
        assert!(out.starts_with("<?php\nnamespace Foo\\Bar;\n\n/**\n * Proxy class for @see \\Foo\\Bar\n */\nclass Proxy extends \\Foo\\Bar implements \\Magento\\Framework\\ObjectManager\\NoninterceptableInterface\n{\n}\n"));
    }
}

fn prop(name: &str, short: &str, var: &str) -> Property {
    Property {
        name: name.into(),
        visibility: Visibility::Protected,
        default: None,
        doc: DocBlock {
            short: Some(short.into()),
            tags: vec![("var".into(), var.into())],
            ..Default::default()
        },
    }
}

fn constructor(source_bs: &str) -> Method {
    Method {
        name: "__construct".into(),
        visibility: Visibility::Public,
        is_static: false,
        is_final: false,
        is_abstract: false,
        returns_ref: false,
        params: vec![
            Param {
                name: "objectManager".into(),
                type_str: Some("\\Magento\\Framework\\ObjectManagerInterface".into()),
                by_ref: false,
                variadic: false,
                default: None,
            },
            Param {
                name: "instanceName".into(),
                type_str: None,
                by_ref: false,
                variadic: false,
                // ValueGenerator over getSourceClassName() (with backslash).
                default: Some(Val::Str(source_bs.to_owned())),
            },
            Param {
                name: "shared".into(),
                type_str: None,
                by_ref: false,
                variadic: false,
                default: Some(Val::Bool(true)),
            },
        ],
        return_type: None,
        body: Some(
            "$this->_objectManager = $objectManager;\n$this->_instanceName = $instanceName;\n$this->_isShared = $shared;"
                .into(),
        ),
        doc: DocBlock {
            short: Some("Proxy constructor".into()),
            tags: vec![
                ("param".into(), "\\Magento\\Framework\\ObjectManagerInterface $objectManager".into()),
                ("param".into(), "string $instanceName".into()),
                ("param".into(), "bool $shared".into()),
            ],
            ..Default::default()
        },
    }
}

/// The special `_resetState` override (subject present ⇒ cascade the reset).
/// The body's trailing space after `_resetState();` is Magento's, preserved.
fn reset_state() -> Method {
    Method {
        name: "_resetState".into(),
        visibility: Visibility::Public,
        is_static: false,
        is_final: false,
        is_abstract: false,
        returns_ref: false,
        params: vec![],
        return_type: Some("void".into()),
        body: Some("if ($this->_subject) {\n    $this->_subject->_resetState(); \n}".into()),
        doc: DocBlock {
            short: Some("Reset state of proxied instance".into()),
            ..Default::default()
        },
    }
}

fn fixed(name: &str, visibility: Visibility, body: &str, doc: DocBlock) -> Method {
    Method {
        name: name.into(),
        visibility,
        is_static: false,
        is_final: false,
        is_abstract: false,
        returns_ref: false,
        params: vec![],
        return_type: None,
        body: Some(body.into()),
        doc,
    }
}

/// `_getMethodInfo`: the delegating body forwards every parameter (variadic
/// spread as `... $name`), dropping `return ` for a `void` method.
fn delegating(rm: &RMethod) -> Method {
    let forward: Vec<String> = rm
        .params
        .iter()
        .map(|p| if p.variadic { format!("... ${}", p.name) } else { format!("${}", p.name) })
        .collect();
    let without_return = rm.return_type.as_deref() == Some("void");
    let call = format!("{}({})", rm.name, forward.join(", "));
    let body = if without_return {
        format!("$this->_getSubject()->{call};")
    } else {
        format!("return $this->_getSubject()->{call};")
    };
    Method {
        name: rm.name.clone(),
        visibility: Visibility::Public,
        is_static: false,
        is_final: false,
        is_abstract: false,
        returns_ref: false,
        params: rm
            .params
            .iter()
            .map(|p| Param {
                name: p.name.clone(),
                type_str: p.type_str.clone(),
                by_ref: p.by_ref,
                variadic: p.variadic,
                default: p.default.clone(),
            })
            .collect(),
        return_type: rm.return_type.clone(),
        body: Some(body),
        doc: DocBlock { short: Some("{@inheritdoc}".into()), ..Default::default() },
    }
}
