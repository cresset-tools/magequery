<?php

namespace Acme\NewDefault\Model\OutOfReach;

use Acme\NewDefault\Model\Value;

/** Out of reach: an internal PHP class (no source). NULL row + a finding. */
class WithInternalClassDefault
{
    public function __construct(?\ArrayObject $v = new \ArrayObject())
    {
    }
}
