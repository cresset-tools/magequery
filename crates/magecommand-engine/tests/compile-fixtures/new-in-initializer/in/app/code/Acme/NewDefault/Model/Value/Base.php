<?php

namespace Acme\NewDefault\Model\Value;

class Base
{
    public const SUFFIX = '!';

    public $pa = 1;
    protected $pb = 'x';
    /** Private: the child's same-named property is a SECOND slot. */
    private $pc = null;
    /** Redeclared by the child: keeps this slot, takes the child's value. */
    public $redecl = 'parent';
    /** Typed, no default: uninitialized, so var_export omits it. */
    public int $typedNoDefault;
    /** Untyped, no default: implicitly null. */
    public $untyped;
    public static $shared = 5;
}
