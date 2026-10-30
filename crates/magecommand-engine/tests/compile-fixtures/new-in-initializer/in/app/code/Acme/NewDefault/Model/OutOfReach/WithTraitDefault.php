<?php

namespace Acme\NewDefault\Model\OutOfReach;

use Acme\NewDefault\Model\Value;

/** Out of reach: the class uses a trait. NULL row + a finding. */
class WithTraitDefault
{
    public function __construct(Value\UsesTrait $v = new Value\UsesTrait())
    {
    }
}
