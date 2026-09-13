#!/usr/bin/env python3
"""Randomised differential fuzzing of `di compile` against real Magento.

Panic-freedom is not the interesting property here. Every divergence found so
far was a confident, correct-looking WRONG answer — a dropped parameter
default, a type rendered in the wrong order, a leaked source-case spelling.
Only an oracle catches those, so this generates random modules, compiles them
with both `bin/magento setup:di:compile` and `magecommand di compile`, and
diffs per namespace.

A Magento compile costs ~90s regardless of how much is in the tree, so the
whole point is BATCHING: one compile covers `--modules` random cases. Each
module gets its own namespace, so a divergence names its own culprit and the
seed reproduces it exactly.

    ./fuzz-against-magento.py --seed 7 --modules 60 --emit /tmp/fz

The generator only emits shapes real Magento accepts. Several legal-looking
constructs abort `setup:di:compile` outright and would cost a whole run:
an `<argument>` with no `xsi:type`, a reference to a generated kind with no
emitter, a plugged `never`-returning method, a plugged method on a plugin
class that does not exist. Those are covered by hand-written fixtures instead
and are deliberately absent here.
"""

import argparse
import pathlib
import random
import shutil

# Type expressions worth combining. Kept to shapes Magento's generator can
# actually render: no DNF (Laminas rejects `A&B|null` — it wants parentheses
# it never emits) and no standalone `null` (rendered `?null`, also rejected).
SCALARS = ["int", "string", "float", "bool", "array", "mixed", "?int", "?string", "iterable"]
RETURNS = ["void", "int", "string", "bool", "array", "static", "self", "?static", "mixed"]

# Default expressions, keyed by the parameter type they are legal for. PHP
# rejects a mismatched default outright ("Cannot use float as default value for
# parameter $x of type ?string"), so a type-blind pool produces files that do
# not even parse and the whole batch is wasted.
#
# `**`, `0.`, `.5` and an enum-ish const inside an array are here deliberately:
# each was a dropped default until recently, and a regression must resurface.
INT_D = ["0", "1", "-1", "PHP_INT_MAX", "self::LIMIT", "2 ** 8", "1 << 3", "7 % 3"]
STR_D = ["'s'", "''", "self::LIMIT . '-x'"]
FLOAT_D = ["0.0", "1.5", "-0.0", "0.1 + 0.2", "1e3", ".5", "0.", "1", "2 ** 8"]
BOOL_D = ["true", "false"]
ARR_D = ["[]", "['a' => 1]", "['a' => ['b' => 2]]", "[self::LIMIT => 'k']", "['f' => 1.5]"]

DEFAULTS_BY_TYPE = {
    "int": INT_D,
    "string": STR_D,
    "float": FLOAT_D,
    "bool": BOOL_D,
    "array": ARR_D,
    "iterable": ["[]"],
    "mixed": INT_D + STR_D + BOOL_D + ARR_D + ["null"],
    "?int": INT_D + ["null"],
    "?string": STR_D + ["null"],
}

SCALARS = list(DEFAULTS_BY_TYPE)
RETURNS = ["void", "int", "string", "bool", "array", "static", "self", "?static", "mixed"]


def default_for(rng, ty):
    """A legal default for `ty`, or None. Class types take only null."""
    if ty.startswith("\\"):
        return None
    pool = DEFAULTS_BY_TYPE.get(ty)
    return rng.choice(pool) if pool else None


XSI_SCALARS = [
    ('string', 'plain'), ('number', '42'), ('number', '1.5'),
    ('boolean', 'true'), ('boolean', '0'), ('const', '{NS}\\Model\\Sub::LIMIT'),
    ('init_parameter', '{NS}\\Model\\Sub::LIMIT'),
]


def rnd_name(rng, prefix):
    return prefix + "".join(rng.choice("abcdefghijklmnopqrstuvwxyz") for _ in range(5)).capitalize()


def gen_method(rng, name):
    """A method signature, plus whether it is safe to plug."""
    n = rng.randint(0, 3)
    required, optional = [], []
    for i in range(n):
        ty = rng.choice(SCALARS)
        by_ref = rng.random() < 0.15
        # A by-ref parameter is passed a variable, so give it no default.
        default = None if by_ref else (
            default_for(rng, ty) if rng.random() < 0.5 else None)
        p = f"{ty} {'&' if by_ref else ''}${rng.choice('abcdefgh')}{i}"
        if default is not None:
            optional.append(f"{p} = {default}")
        else:
            required.append(p)
    # A required parameter after an optional one is deprecated in PHP 8 and
    # makes every earlier one implicitly required; keep the order legal.
    params = required + optional
    if rng.random() < 0.12:
        params.append(f"{rng.choice(['int', 'string', 'mixed'])} ...$rest")
    ret = rng.choice(RETURNS)
    body = {
        "void": "", "static": "return $this;", "self": "return $this;",
        "?static": "return null;", "int": "return 0;", "string": "return '';",
        "bool": "return true;", "array": "return [];", "mixed": "return null;",
    }[ret]
    sig = f"    public function {name}({', '.join(params)}): {ret}\n    {{\n        {body}\n    }}"
    return sig, ret


def gen_module(rng, index, out: pathlib.Path):
    mod = f"Fz{index:03d}"
    name = f"Acme_{mod}"
    ns = f"Acme\\{mod}"
    root = out / "Acme" / mod
    (root / "Model").mkdir(parents=True, exist_ok=True)
    (root / "etc").mkdir(parents=True, exist_ok=True)

    (root / "registration.php").write_text(
        "<?php\n\n\\Magento\\Framework\\Component\\ComponentRegistrar::register(\n"
        "    \\Magento\\Framework\\Component\\ComponentRegistrar::MODULE,\n"
        f"    '{name}',\n    __DIR__\n);\n")
    (root / "etc" / "module.xml").write_text(
        '<?xml version="1.0"?>\n<config xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" '
        'xsi:noNamespaceSchemaLocation="urn:magento:framework:Module/etc/module.xsd">\n'
        f'    <module name="{name}"/>\n</config>\n')

    # A dependency and a subject class.
    (root / "Model" / "Sub.php").write_text(
        f"<?php\n\nnamespace {ns}\\Model;\n\nclass Sub\n{{\n"
        f"    public const LIMIT = {rng.randint(1, 99)};\n}}\n")

    methods, plugables = [], []
    for i in range(rng.randint(1, 4)):
        mname = f"do{rng.choice('XYZWQ')}{i}"
        sig, ret = gen_method(rng, mname)
        methods.append(sig)
        plugables.append((mname, ret))

    ctor_req, ctor_opt, scalar_params = [], [], []
    for i in range(rng.randint(0, 3)):
        ty = rng.choice(SCALARS + [f"\\{ns}\\Model\\Sub"])
        pname = f"{rng.choice('pqrstu')}{i}"
        d = default_for(rng, ty) if rng.random() < 0.6 else None
        p = f"        private {ty} ${pname}"
        if d is not None:
            ctor_opt.append(f"{p} = {d}")
        else:
            ctor_req.append(p)
        if not ty.startswith("\\"):
            scalar_params.append(pname)
    ctor_params = ctor_req + ctor_opt
    ctor = ""
    if ctor_params:
        ctor = ("    public function __construct(\n" + ",\n".join(ctor_params)
                + "\n    ) {\n    }\n\n")

    modifiers = rng.choice(["", "", "", "abstract ", "final "])
    subject = f"{ns}\\Model\\Target"

    # Sometimes an interface the class implements, so a preference has a `for`.
    iface = None
    if rng.random() < 0.45:
        iface = "TargetInterface"
        (root / "Api").mkdir(exist_ok=True)
        (root / "Api" / f"{iface}.php").write_text(
            f"<?php\n\nnamespace {ns}\\Api;\n\ninterface {iface}\n{{\n"
            f"    public function iface(): void;\n}}\n")
        methods.append("    public function iface(): void\n    {\n    }")
        plugables.append(("iface", "void"))

    # Sometimes a base class, so the constructor is INHERITED rather than
    # declared and the leaf's interceptor must still forward every parameter.
    # Sometimes THREE levels, the middle declaring nothing, so the leaf
    # inherits through it and di.xml may configure any level.
    extends = ""
    chain = []
    if ctor and rng.random() < 0.4:
        (root / "Model" / "Base.php").write_text(
            f"<?php\n\nnamespace {ns}\\Model;\n\nclass Base\n{{\n"
            f"    public const LIMIT = {rng.randint(1, 50)};\n\n{ctor}}}\n")
        chain.append("Base")
        if rng.random() < 0.5:
            (root / "Model" / "Mid.php").write_text(
                f"<?php\n\nnamespace {ns}\\Model;\n\n"
                "/** No constructor: the leaf inherits Base's through here. */\n"
                "class Mid extends Base\n{\n}\n")
            chain.append("Mid")
        extends, ctor = f" extends {chain[-1]}", ""

    impl = f" implements \\{ns}\\Api\\{iface}" if iface else ""
    (root / "Model" / "Target.php").write_text(
        f"<?php\n\nnamespace {ns}\\Model;\n\n{modifiers}class Target{extends}{impl}\n{{\n"
        f"    public const LIMIT = {rng.randint(1, 50)};\n\n"
        + ctor + "\n\n".join(methods) + "\n}\n")

    # di.xml: a plugin (only on a concrete class), arguments, a virtualType.
    di = ['<?xml version="1.0"?>',
          '<config xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" '
          'xsi:noNamespaceSchemaLocation="urn:magento:framework:ObjectManager/etc/config.xsd">']
    args = []
    # Only a scalar-typed parameter may take a scalar argument. Aiming one at a
    # CLASS-typed parameter is fatal to the real compile: ArgumentsResolver
    # reads `$config['instance']` off the string and dies, taking the round
    # with it.
    for pname in rng.sample(scalar_params, min(len(scalar_params), rng.randint(0, 3))):
        kind, val = rng.choice(XSI_SCALARS)
        val = val.replace("{NS}", ns)
        args.append(f'            <argument name="{pname}" '
                    f'xsi:type="{kind}">{val}</argument>')
    # An array argument needs an array-typed parameter to land on; `arr` is
    # declared for exactly that purpose below.
    if rng.random() < 0.4:
        args.append('            <argument name="arr" xsi:type="array">\n'
                    '                <item name="k" xsi:type="string">v</item>\n'
                    '                <item name="0" xsi:type="number">7</item>\n'
                    '            </argument>')
        ctor_params.append("        private array $arr = []")
    plug = ""
    # Only a PLAIN class may be plugged. An abstract one is filtered by
    # InheritanceInterceptorScanner, but a plugin declared directly on a FINAL
    # class is not: Magento happily generates
    # `class Target\Interceptor extends Target` and PHP then refuses —
    # "cannot extend final class" — aborting the whole compile. That is an
    # upstream bug, and generating it would cost the batch, so it is excluded
    # here and left to a hand-written fixture.
    if modifiers.strip() == "" and plugables and rng.random() < 0.85:
        (root / "Plugin").mkdir(exist_ok=True)
        decls = []
        # Several plugins, deliberately including sortOrder TIES (uasort's
        # comparator returns 0 and PHP 8 sorts stably, so the tie-break is
        # insertion order) and the occasional disabled one.
        for pi in range(rng.randint(1, 3)):
            cls = f"P{pi}"
            mname, ret = rng.choice(plugables)
            kind = rng.choice(["after", "after", "before", "around"])
            cap = mname[0].upper() + mname[1:]
            if kind == "after":
                body = (f"    public function after{cap}($subject, $result)\n"
                        f"    {{\n        return $result;\n    }}\n")
            elif kind == "before":
                body = (f"    public function before{cap}($subject, ...$args): array\n"
                        f"    {{\n        return $args;\n    }}\n")
            else:
                body = (f"    public function around{cap}($subject, callable $proceed, "
                        f"...$args)\n    {{\n        return $proceed(...$args);\n    }}\n")
            (root / "Plugin" / f"{cls}.php").write_text(
                f"<?php\n\nnamespace {ns}\\Plugin;\n\nclass {cls}\n{{\n{body}}}\n")
            attrs = f'sortOrder="{rng.choice([1, 10, 10, 10, 20, 30])}"'
            if rng.random() < 0.12:
                attrs += ' disabled="true"'
            decls.append(f'        <plugin name="fz{index}_p{pi}" '
                         f'type="{ns}\\Plugin\\{cls}" {attrs}/>')
        plug = "\n".join(decls)

    # The subject's own <type> node. This is the main construct — without it a
    # module carries no DI config at all, and a round proves nothing.
    # Configure an ANCESTOR too. Own-vs-inherited merges with
    # array_replace_recursive, but merging ACROSS relations is a plain
    # array_replace, so which level declares what changes the answer.
    if chain and args and rng.random() < 0.5:
        di.append(f'    <type name="{ns}\\Model\\{chain[0]}">')
        di.append("        <arguments>")
        di.extend(args[-1:])
        di.append("        </arguments>")
        di.append("    </type>")

    if args or plug:
        di.append(f'    <type name="{subject}">')
        if args:
            di.append("        <arguments>")
            di.extend(args)
            di.append("        </arguments>")
        if plug:
            di.append(plug)
        di.append("    </type>")

    # A virtualType, sometimes carrying its own arguments (which inherit the
    # base's and override per key) and sometimes chained off another.
    if rng.random() < 0.4 and modifiers.strip() != "abstract":
        vt = f"Fz{index}Virtual"
        if args and rng.random() < 0.5:
            di.append(f'    <virtualType name="{vt}" type="{subject}">')
            di.append("        <arguments>")
            di.extend(args[:1])
            di.append("        </arguments>")
            di.append("    </virtualType>")
        else:
            di.append(f'    <virtualType name="{vt}" type="{subject}"/>')
        if rng.random() < 0.4:
            di.append(f'    <virtualType name="{vt}Derived" type="{vt}"/>')

    # A preference pointing the interface at the concrete class.
    if iface and modifiers.strip() == "":
        di.append(f'    <preference for="{ns}\\Api\\{iface}" type="{subject}"/>')
    di.append("</config>\n")
    (root / "etc" / "di.xml").write_text("\n".join(di))

    # An AREA-scoped di.xml: another plugin on the same subject, so the area
    # plugin-list files diverge from global.
    if plug and rng.random() < 0.4:
        area = rng.choice(["frontend", "adminhtml", "crontab", "graphql"])
        mname, _ = rng.choice(plugables)
        cap = mname[0].upper() + mname[1:]
        (root / "Plugin" / "Area.php").write_text(
            f"<?php\n\nnamespace {ns}\\Plugin;\n\nclass Area\n{{\n"
            f"    public function after{cap}($subject, $result)\n"
            f"    {{\n        return $result;\n    }}\n}}\n")
        (root / "etc" / area).mkdir(parents=True, exist_ok=True)
        (root / "etc" / area / "di.xml").write_text(
            '<?xml version="1.0"?>\n'
            '<config xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" '
            'xsi:noNamespaceSchemaLocation="urn:magento:framework:ObjectManager/etc/config.xsd">\n'
            f'    <type name="{subject}">\n'
            f'        <plugin name="fz{index}_area" type="{ns}\\Plugin\\Area" '
            f'sortOrder="{rng.choice([5, 15, 25])}"/>\n'
            '    </type>\n</config>\n')
    # An OVERLAY module sequenced AFTER this one, re-configuring the same
    # type. Merge order is only reachable with a coordinated PAIR — independent
    # modules never collide, which is why the generator could not reach it
    # before. Pins that a later argument wins per KEY (earlier keys survive),
    # that a same-named plugin MERGES rather than duplicating, and that a later
    # preference wins outright.
    if (args or plug) and rng.random() < 0.35:
        omod, oname = f"{mod}b", f"Acme_{mod}b"
        oroot = out / "Acme" / omod
        (oroot / "etc").mkdir(parents=True, exist_ok=True)
        (oroot / "registration.php").write_text(
            "<?php\n\n\\Magento\\Framework\\Component\\ComponentRegistrar::register(\n"
            "    \\Magento\\Framework\\Component\\ComponentRegistrar::MODULE,\n"
            f"    '{oname}',\n    __DIR__\n);\n")
        (oroot / "etc" / "module.xml").write_text(
            '<?xml version="1.0"?>\n<config xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" '
            'xsi:noNamespaceSchemaLocation="urn:magento:framework:Module/etc/module.xsd">\n'
            f'    <module name="{oname}">\n        <sequence>\n'
            f'            <module name="{name}"/>\n'
            "        </sequence>\n    </module>\n</config>\n")
        od = ['<?xml version="1.0"?>',
              '<config xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" '
              'xsi:noNamespaceSchemaLocation="urn:magento:framework:ObjectManager/etc/config.xsd">',
              f'    <type name="{subject}">']
        if args:
            aname = args[0].split('name="', 1)[1].split('"', 1)[0]
            kind, val = rng.choice(XSI_SCALARS)
            val = val.replace("{NS}", ns)
            od.append("        <arguments>")
            od.append(f'            <argument name="{aname}" xsi:type="{kind}">{val}</argument>')
            od.append("        </arguments>")
        if plug:
            pname = plug.split('name="', 1)[1].split('"', 1)[0]
            od.append(f'        <plugin name="{pname}" sortOrder="{rng.choice([2, 40, 99])}"/>')
        od.append("    </type>")
        if iface and modifiers.strip() == "":
            od.append(f'    <preference for="{ns}\\Api\\{iface}" type="{subject}"/>')
        od.append("</config>\n")
        (oroot / "etc" / "di.xml").write_text("\n".join(od))

    return name, ns


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--modules", type=int, default=40)
    ap.add_argument("--emit", required=True, help="directory to write app/code into")
    args = ap.parse_args()

    rng = random.Random(args.seed)
    out = pathlib.Path(args.emit)
    if (out / "Acme").exists():
        shutil.rmtree(out / "Acme")
    out.mkdir(parents=True, exist_ok=True)

    names = []
    for i in range(args.modules):
        n, _ = gen_module(rng, i, out)
        names.append(n)
    print("\n".join(names))


if __name__ == "__main__":
    main()
