<?php

namespace Acme\NewDefault\Model\OutOfReach;

use Acme\NewDefault\Model\Value;

/** Out of reach: the class declares a constructor. NULL row + a finding. */
class WithConstructorDefault
{
    public function __construct(Value\WithConstructor $v = new Value\WithConstructor())
    {
    }
}
