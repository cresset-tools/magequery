<?php

namespace Acme\NewDefault\Model\Value;

/** A property default that does not fold statically (machine-dependent). */
class Unfoldable
{
    public $os = PHP_OS;
}
