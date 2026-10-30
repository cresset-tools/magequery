<?php

namespace Acme\NewDefault\Model\OutOfReach;

use Acme\NewDefault\Model\Value;

/** Out of reach: an ancestor declares a constructor. NULL row + a finding. */
class WithInheritedConstructorDefault
{
    public function __construct(Value\InheritsConstructor $v = new Value\InheritsConstructor())
    {
    }
}
